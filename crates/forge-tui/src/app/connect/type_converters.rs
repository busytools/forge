//! Wire-shape conversions from `forge_primitives::*` (the
//! serde-derived envelope structs) into `crate::agent::model::*`
//! (the App's runtime model). Consumed by the App-side `sdk_message`
//! reducer and slash-command executors that build model values
//! from wire envelopes captured in tool-call payloads.

use crate::agent::model;
use forge_primitives as types;

pub(crate) fn map_available_commands_update(
    commands: Vec<types::AvailableCommand>,
) -> model::AvailableCommandsUpdate {
    // model::AvailableCommand == primitives::AvailableCommand now.
    // Drop empty input_hint strings on the boundary; keep the rest.
    model::AvailableCommandsUpdate::new(
        commands
            .into_iter()
            .map(|mut cmd| {
                if cmd.input_hint.as_deref().is_some_and(|h| h.trim().is_empty()) {
                    cmd.input_hint = None;
                }
                cmd
            })
            .collect(),
    )
}

pub(crate) fn map_available_agents_update(
    agents: Vec<types::AvailableAgent>,
) -> model::AvailableAgentsUpdate {
    // model::AvailableAgent == primitives::AvailableAgent now. Drop
    // empty model strings on the boundary.
    model::AvailableAgentsUpdate::new(
        agents
            .into_iter()
            .map(|mut agent| {
                if agent.model.as_deref().is_some_and(|m| m.trim().is_empty()) {
                    agent.model = None;
                }
                agent
            })
            .collect(),
    )
}

pub(crate) fn map_available_models(
    models: Vec<types::AvailableModel>,
) -> Vec<model::AvailableModel> {
    // model::AvailableModel == primitives::AvailableModel. Strip empty
    // description strings; everything else passes through unchanged.
    models
        .into_iter()
        .map(|mut m| {
            if m.description.as_deref().is_some_and(|d| d.trim().is_empty()) {
                m.description = None;
            }
            m
        })
        .collect()
}

pub(crate) fn map_permission_request(
    session_id: &str,
    request: types::PermissionRequest,
) -> (model::RequestPermissionRequest, String) {
    let tool_call_id = request.tool_call.tool_call_id.clone();
    let tool_call_meta = request.tool_call.meta.clone();
    let tool_call_fields = convert_tool_call_to_fields(request.tool_call);
    let mut tool_call_update = model::ToolCallUpdate::new(tool_call_id.clone(), tool_call_fields);
    if let Some(meta) = tool_call_meta {
        tool_call_update = tool_call_update.meta(meta);
    }
    let options = request
        .options
        .into_iter()
        .map(|opt| {
            let kind = match opt.kind.as_str() {
                "allow_once" => model::PermissionOptionKind::AllowOnce,
                "allow_session" => model::PermissionOptionKind::AllowSession,
                "allow_always" => model::PermissionOptionKind::AllowAlways,
                "reject_once" => model::PermissionOptionKind::RejectOnce,
                "reject_always" => model::PermissionOptionKind::RejectAlways,
                "question_choice" => model::PermissionOptionKind::QuestionChoice,
                "plan_approve" => model::PermissionOptionKind::PlanApprove,
                "plan_reject" => model::PermissionOptionKind::PlanReject,
                _ => {
                    tracing::warn!(
                        "unknown permission option kind from bridge; defaulting to reject_once: session_id={} tool_call_id={} option_id={} option_name={} option_kind={}",
                        session_id,
                        tool_call_id,
                        opt.option_id,
                        opt.name,
                        opt.kind
                    );
                    model::PermissionOptionKind::RejectOnce
                }
            };
            model::PermissionOption::new(opt.option_id, opt.name, kind).description(opt.description)
        })
        .collect();
    (
        model::RequestPermissionRequest::new(
            model::SessionId::new(session_id),
            tool_call_update,
            options,
            request.display.filter(|d| !d.is_empty()),
        ),
        tool_call_id,
    )
}

pub(crate) fn map_question_request(
    session_id: &str,
    request: types::QuestionRequest,
) -> (model::RequestQuestionRequest, String) {
    let tool_call_id = request.tool_call.tool_call_id.clone();
    let tool_call_meta = request.tool_call.meta.clone();
    let tool_call_fields = convert_tool_call_to_fields(request.tool_call);
    let mut tool_call_update = model::ToolCallUpdate::new(tool_call_id.clone(), tool_call_fields);
    if let Some(meta) = tool_call_meta {
        tool_call_update = tool_call_update.meta(meta);
    }

    let prompt = model::QuestionPrompt::new(
        request.prompt.question,
        request.prompt.header,
        request.prompt.multi_select,
        request
            .prompt
            .options
            .into_iter()
            .map(|option| {
                model::QuestionOption::new(option.option_id, option.label)
                    .description(option.description)
                    .preview(option.preview)
            })
            .collect(),
    );

    (
        model::RequestQuestionRequest::new(
            model::SessionId::new(session_id),
            tool_call_update,
            prompt,
            usize::try_from(request.question_index).unwrap_or(0),
            usize::try_from(request.total_questions).unwrap_or(0),
        ),
        tool_call_id,
    )
}

pub(super) fn convert_content_block(content: types::ChunkContent) -> Option<model::ContentBlock> {
    match content {
        types::ChunkContent::Text { text } => {
            Some(model::ContentBlock::Text(model::TextContent::new(text)))
        }
        types::ChunkContent::Image { mime_type, uri: _, data } => {
            let mime = mime_type.unwrap_or_else(|| "image/png".to_owned());
            let image_data = data.unwrap_or_default();
            if !forge_primitives::image::is_supported_image_type(&mime) {
                tracing::warn!(mime_type = %mime, "convert_content_block: skipping unsupported image type");
                return None;
            }
            if image_data.is_empty() {
                tracing::warn!("convert_content_block: skipping image block with empty data");
                return None;
            }
            Some(model::ContentBlock::Image(model::ImageContent::new(image_data, mime)))
        }
    }
}

pub(crate) fn convert_tool_call(tool_call: types::ToolCall) -> model::ToolCall {
    let types::ToolCall {
        tool_call_id,
        title,
        kind,
        status,
        content,
        raw_input,
        raw_output,
        output_metadata,
        task_metadata,
        locations,
        meta,
    } = tool_call;

    let mut tc = model::ToolCall::new(tool_call_id, title)
        .kind(convert_tool_kind(&kind))
        .status(convert_tool_status(&status))
        .content(content.into_iter().filter_map(convert_tool_call_content).collect())
        .locations(
            locations
                .into_iter()
                .map(|loc| {
                    let mut location = model::ToolCallLocation::new(loc.path);
                    if let Some(line) = loc.line.and_then(|line| u32::try_from(line).ok()) {
                        location = location.line(line);
                    }
                    location
                })
                .collect(),
        );

    if let Some(raw_input) = raw_input {
        tc = tc.raw_input(raw_input);
    }

    if let Some(raw_output) = raw_output {
        tc = tc.raw_output(serde_json::Value::String(raw_output));
    }
    if let Some(output_metadata) = output_metadata {
        tc = tc.output_metadata(output_metadata);
    }
    if let Some(task_metadata) = task_metadata {
        tc = tc.task_metadata(task_metadata);
    }
    if let Some(meta) = meta {
        tc = tc.meta(meta);
    }

    tc
}

pub(crate) fn convert_tool_call_update(update: types::ToolCallUpdate) -> model::ToolCallUpdate {
    let update_meta = update.fields.meta.clone();
    let mut out = model::ToolCallUpdate::new(
        update.tool_call_id,
        convert_tool_call_update_fields(update.fields),
    );
    if let Some(meta) = update_meta {
        out = out.meta(meta);
    }
    out
}

pub(super) fn convert_tool_call_to_fields(
    tool_call: types::ToolCall,
) -> model::ToolCallUpdateFields {
    let mut fields = model::ToolCallUpdateFields::new()
        .title(tool_call.title)
        .kind(convert_tool_kind(&tool_call.kind))
        .status(convert_tool_status(&tool_call.status))
        .content(
            tool_call.content.into_iter().filter_map(convert_tool_call_content).collect::<Vec<_>>(),
        )
        .locations(
            tool_call
                .locations
                .into_iter()
                .map(|loc| {
                    let mut location = model::ToolCallLocation::new(loc.path);
                    if let Some(line) = loc.line.and_then(|line| u32::try_from(line).ok()) {
                        location = location.line(line);
                    }
                    location
                })
                .collect::<Vec<_>>(),
        );

    if let Some(raw_input) = tool_call.raw_input {
        fields = fields.raw_input(raw_input);
    }

    if let Some(raw_output) = tool_call.raw_output {
        fields = fields.raw_output(serde_json::Value::String(raw_output));
    }
    if let Some(output_metadata) = tool_call.output_metadata {
        fields = fields.output_metadata(output_metadata);
    }
    if let Some(task_metadata) = tool_call.task_metadata {
        fields = fields.task_metadata(task_metadata);
    }

    fields
}

pub(super) fn convert_tool_call_update_fields(
    fields: types::ToolCallUpdateFields,
) -> model::ToolCallUpdateFields {
    let mut out = model::ToolCallUpdateFields::new();

    if let Some(title) = fields.title {
        out = out.title(title);
    }
    if let Some(kind) = fields.kind {
        out = out.kind(convert_tool_kind(&kind));
    }
    if let Some(status) = fields.status {
        out = out.status(convert_tool_status(&status));
    }
    if let Some(content) = fields.content {
        out = out
            .content(content.into_iter().filter_map(convert_tool_call_content).collect::<Vec<_>>());
    }
    if let Some(raw_input) = fields.raw_input {
        out = out.raw_input(raw_input);
    }
    if let Some(raw_output) = fields.raw_output {
        out = out.raw_output(serde_json::Value::String(raw_output));
    }
    if let Some(output_metadata) = fields.output_metadata {
        out = out.output_metadata(output_metadata);
    }
    if let Some(task_metadata) = fields.task_metadata {
        out = out.task_metadata(task_metadata);
    }
    if let Some(locations) = fields.locations {
        out = out.locations(
            locations
                .into_iter()
                .map(|loc| {
                    let mut location = model::ToolCallLocation::new(loc.path);
                    if let Some(line) = loc.line.and_then(|line| u32::try_from(line).ok()) {
                        location = location.line(line);
                    }
                    location
                })
                .collect::<Vec<_>>(),
        );
    }

    out
}


fn convert_tool_call_content(
    tool_content: types::ToolCallContent,
) -> Option<model::ToolCallContent> {
    match tool_content {
        types::ToolCallContent::Content { content } => {
            let block = convert_content_block(content)?;
            Some(model::ToolCallContent::Content(model::Content::new(block)))
        }
        types::ToolCallContent::Diff { new_path, old, new, repository } => {
            Some(model::ToolCallContent::Diff(
                model::Diff::new(new_path, new).old_text(Some(old)).repository(repository),
            ))
        }
        types::ToolCallContent::McpResource { uri, mime_type, text, blob_saved_to } => {
            Some(model::ToolCallContent::McpResource(
                model::McpResource::new(uri)
                    .mime_type(mime_type)
                    .text(text)
                    .blob_saved_to(blob_saved_to),
            ))
        }
    }
}

pub(super) fn convert_tool_kind(kind: &str) -> model::ToolKind {
    match kind {
        "read" => model::ToolKind::Read,
        "edit" => model::ToolKind::Edit,
        "delete" => model::ToolKind::Delete,
        "move" => model::ToolKind::Move,
        "execute" => model::ToolKind::Execute,
        "search" => model::ToolKind::Search,
        "fetch" => model::ToolKind::Fetch,
        "switch_mode" => model::ToolKind::SwitchMode,
        "other" => model::ToolKind::Other,
        _ => model::ToolKind::Think,
    }
}

pub(super) fn convert_tool_status(status: &str) -> model::ToolCallStatus {
    match status {
        "in_progress" => model::ToolCallStatus::InProgress,
        "completed" => model::ToolCallStatus::Completed,
        "failed" => model::ToolCallStatus::Failed,
        "killed" => model::ToolCallStatus::Killed,
        _ => model::ToolCallStatus::Pending,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        convert_tool_call, convert_tool_call_update_fields, map_available_models,
        map_permission_request, map_question_request,
    };
    use crate::agent::model;
    use forge_primitives as types;

    #[test]
    fn map_available_models_preserves_optional_fast_and_auto_metadata() {
        let mapped = map_available_models(vec![
            types::AvailableModel {
                id: "sonnet".to_owned(),
                display_name: "Claude Sonnet".to_owned(),
                description: Some("Balanced model".to_owned()),
                supports_effort: true,
                supported_effort_levels: vec![
                    types::EffortLevel::Low,
                    types::EffortLevel::Medium,
                    types::EffortLevel::High,
                ],
                supports_adaptive_thinking: Some(true),
                supports_fast_mode: Some(true),
                supports_auto_mode: Some(false),
            },
            types::AvailableModel {
                id: "haiku".to_owned(),
                display_name: "Claude Haiku".to_owned(),
                description: None,
                supports_effort: false,
                supported_effort_levels: Vec::new(),
                supports_adaptive_thinking: None,
                supports_fast_mode: None,
                supports_auto_mode: None,
            },
        ]);

        assert_eq!(
            mapped,
            vec![
                model::AvailableModel::new("sonnet", "Claude Sonnet")
                    .description("Balanced model")
                    .supports_effort(true)
                    .supported_effort_levels(vec![
                        model::EffortLevel::Low,
                        model::EffortLevel::Medium,
                        model::EffortLevel::High,
                    ])
                    .supports_adaptive_thinking(Some(true))
                    .supports_fast_mode(Some(true))
                    .supports_auto_mode(Some(false)),
                model::AvailableModel::new("haiku", "Claude Haiku")
                    .supports_adaptive_thinking(None)
                    .supports_fast_mode(None)
                    .supports_auto_mode(None),
            ]
        );
    }

    #[test]
    fn map_permission_request_preserves_display_metadata() {
        let (request, tool_call_id) = map_permission_request(
            "session-1",
            types::PermissionRequest {
                tool_call: types::ToolCall {
                    tool_call_id: "tool-1".to_owned(),
                    title: "Bash npm test".to_owned(),
                    kind: "execute".to_owned(),
                    status: "in_progress".to_owned(),
                    content: Vec::new(),
                    raw_input: None,
                    raw_output: None,
                    output_metadata: None,
                    task_metadata: None,
                    locations: Vec::new(),
                    meta: None,
                },
                options: vec![types::PermissionOption {
                    option_id: "allow".to_owned(),
                    name: "Allow".to_owned(),
                    description: None,
                    kind: "allow_once".to_owned(),
                }],
                display: Some(types::PermissionDisplay {
                    title: Some("Claude wants to run tests".to_owned()),
                    display_name: Some("Run tests".to_owned()),
                    description: Some("This command reads project files".to_owned()),
                }),
            },
        );

        assert_eq!(tool_call_id, "tool-1");
        assert_eq!(
            request.display,
            Some(
                model::PermissionDisplay::new()
                    .title(Some("Claude wants to run tests".to_owned()))
                    .display_name(Some("Run tests".to_owned()))
                    .description(Some("This command reads project files".to_owned())),
            )
        );
    }

    #[test]
    fn map_question_request_preserves_preview_and_annotation_shape() {
        let (request, tool_call_id) = map_question_request(
            "session-1",
            types::QuestionRequest {
                tool_call: types::ToolCall {
                    tool_call_id: "tool-1".to_owned(),
                    title: "Pick target".to_owned(),
                    kind: "other".to_owned(),
                    status: "in_progress".to_owned(),
                    content: Vec::new(),
                    raw_input: Some(serde_json::json!({ "source": "ask_user_question" })),
                    raw_output: None,
                    output_metadata: None,
                    task_metadata: None,
                    locations: Vec::new(),
                    meta: Some(
                        serde_json::json!({ "claudeCode": { "toolName": "AskUserQuestion" } }),
                    ),
                },
                prompt: types::QuestionPrompt {
                    question: "Where should this roll out?".to_owned(),
                    header: "Target".to_owned(),
                    multi_select: true,
                    options: vec![
                        types::QuestionOption {
                            option_id: "question_0".to_owned(),
                            label: "Staging".to_owned(),
                            description: Some("Validate in staging first".to_owned()),
                            preview: Some("Deploy to staging first.".to_owned()),
                        },
                        types::QuestionOption {
                            option_id: "question_1".to_owned(),
                            label: "Production".to_owned(),
                            description: Some("Customer-facing rollout".to_owned()),
                            preview: None,
                        },
                    ],
                },
                question_index: 1,
                total_questions: 3,
            },
        );

        assert_eq!(tool_call_id, "tool-1");
        assert_eq!(
            request,
            model::RequestQuestionRequest::new(
                model::SessionId::new("session-1"),
                model::ToolCallUpdate::new(
                    "tool-1",
                    model::ToolCallUpdateFields::new()
                        .title("Pick target")
                        .kind(model::ToolKind::Other)
                        .status(model::ToolCallStatus::InProgress)
                        .content(Vec::new())
                        .raw_input(serde_json::json!({ "source": "ask_user_question" }))
                        .locations(Vec::new()),
                )
                .meta(serde_json::json!({ "claudeCode": { "toolName": "AskUserQuestion" } })),
                model::QuestionPrompt::new(
                    "Where should this roll out?",
                    "Target",
                    true,
                    vec![
                        model::QuestionOption::new("question_0", "Staging")
                            .description(Some("Validate in staging first".to_owned()))
                            .preview(Some("Deploy to staging first.".to_owned())),
                        model::QuestionOption::new("question_1", "Production")
                            .description(Some("Customer-facing rollout".to_owned()))
                            .preview(None),
                    ],
                ),
                1,
                3,
            )
        );
    }

    #[test]
    fn convert_tool_call_update_fields_preserves_output_metadata() {
        let fields = convert_tool_call_update_fields(types::ToolCallUpdateFields {
            status: Some("completed".to_owned()),
            output_metadata: Some(types::ToolOutputMetadata {
                bash: Some(types::BashOutputMetadata { assistant_auto_backgrounded: Some(true) }),
                todo_write: Some(types::TodoWriteOutputMetadata {
                    verification_nudge_needed: Some(true),
                }),
            }),
            ..types::ToolCallUpdateFields::default()
        });

        assert_eq!(
            fields.output_metadata,
            Some(
                model::ToolOutputMetadata::new()
                    .bash(Some(
                        model::BashOutputMetadata::new().assistant_auto_backgrounded(Some(true)),
                    ))
                    .todo_write(Some(
                        model::TodoWriteOutputMetadata::new().verification_nudge_needed(Some(true)),
                    )),
            )
        );
    }

    #[test]
    fn convert_tool_status_maps_killed() {
        assert_eq!(super::convert_tool_status("killed"), model::ToolCallStatus::Killed);
    }

    #[test]
    fn convert_tool_call_update_fields_preserves_task_metadata() {
        let fields = convert_tool_call_update_fields(types::ToolCallUpdateFields {
            task_metadata: Some(types::TaskMetadata {
                end_time: Some(123),
                total_paused_ms: Some(45),
                error: Some("Task stopped".to_owned()),
                is_backgrounded: Some(true),
            }),
            ..types::ToolCallUpdateFields::default()
        });

        assert_eq!(
            fields.task_metadata,
            Some(
                model::TaskMetadata::new()
                    .end_time(Some(123))
                    .total_paused_ms(Some(45))
                    .error(Some("Task stopped".to_owned()))
                    .backgrounded(Some(true)),
            )
        );
    }

    #[test]
    fn convert_tool_call_preserves_task_metadata() {
        let tool_call = convert_tool_call(types::ToolCall {
            tool_call_id: "tool-task".to_owned(),
            title: "Agent task".to_owned(),
            kind: "think".to_owned(),
            status: "killed".to_owned(),
            content: Vec::new(),
            raw_input: None,
            raw_output: None,
            output_metadata: None,
            task_metadata: Some(types::TaskMetadata {
                end_time: Some(77),
                total_paused_ms: Some(11),
                error: Some("Task stopped".to_owned()),
                is_backgrounded: Some(false),
            }),
            locations: Vec::new(),
            meta: None,
        });

        assert_eq!(tool_call.status, model::ToolCallStatus::Killed);
        assert_eq!(
            tool_call.task_metadata,
            Some(
                model::TaskMetadata::new()
                    .end_time(Some(77))
                    .total_paused_ms(Some(11))
                    .error(Some("Task stopped".to_owned()))
                    .backgrounded(Some(false)),
            )
        );
    }

    #[test]
    fn convert_tool_call_preserves_diff_repository() {
        let tool_call = convert_tool_call(types::ToolCall {
            tool_call_id: "tool-1".to_owned(),
            title: "Write src/main.rs".to_owned(),
            kind: "edit".to_owned(),
            status: "completed".to_owned(),
            content: vec![types::ToolCallContent::Diff {
                new_path: "src/main.rs".to_owned(),
                old: "old".to_owned(),
                new: "new".to_owned(),
                repository: Some("acme/project".to_owned()),
            }],
            raw_input: None,
            raw_output: None,
            output_metadata: None,
            task_metadata: None,
            locations: Vec::new(),
            meta: None,
        });

        assert_eq!(
            tool_call.content,
            vec![model::ToolCallContent::Diff(
                model::Diff::new("src/main.rs", "new")
                    .old_text(Some("old"))
                    .repository(Some("acme/project".to_owned())),
            )]
        );
    }

    #[test]
    fn convert_tool_call_preserves_mcp_resource_blob_path() {
        let tool_call = convert_tool_call(types::ToolCall {
            tool_call_id: "tool-2".to_owned(),
            title: "ReadMcpResource docs file://manual.pdf".to_owned(),
            kind: "read".to_owned(),
            status: "completed".to_owned(),
            content: vec![types::ToolCallContent::McpResource {
                uri: "file://manual.pdf".to_owned(),
                mime_type: Some("application/pdf".to_owned()),
                text: Some(
                    "[Resource from docs at file://manual.pdf] Saved to C:\\tmp\\manual.pdf"
                        .to_owned(),
                ),
                blob_saved_to: Some("C:\\tmp\\manual.pdf".to_owned()),
            }],
            raw_input: None,
            raw_output: None,
            output_metadata: None,
            task_metadata: None,
            locations: Vec::new(),
            meta: None,
        });

        assert_eq!(
            tool_call.content,
            vec![model::ToolCallContent::McpResource(
                model::McpResource::new("file://manual.pdf")
                    .mime_type(Some("application/pdf".to_owned()))
                    .text(Some(
                        "[Resource from docs at file://manual.pdf] Saved to C:\\tmp\\manual.pdf"
                            .to_owned(),
                    ))
                    .blob_saved_to(Some("C:\\tmp\\manual.pdf".to_owned())),
            )]
        );
    }
}
