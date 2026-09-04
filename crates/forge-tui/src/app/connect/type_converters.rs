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

pub(super) fn convert_content_block(
    content: types::ChunkContent,
) -> Option<model::RenderContentBlock> {
    match content {
        types::ChunkContent::Text { text } => {
            Some(model::RenderContentBlock::Text(model::TextContent::new(text)))
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
            Some(model::RenderContentBlock::Image(model::ImageContent::new(image_data, mime)))
        }
    }
}

pub(crate) fn convert_tool_call(tool_call: types::ToolCall) -> model::RenderToolCall {
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

    let mut tc = model::RenderToolCall::new(tool_call_id, title)
        .kind(kind)
        .status(status)
        .content(content.into_iter().filter_map(convert_tool_call_content).collect())
        .locations(locations);

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

pub(crate) fn convert_tool_call_update(
    update: types::ToolCallUpdate,
) -> model::RenderToolCallUpdate {
    // Exhaustive destructure (no `..`) so a new wire field fails the
    // build until it's mapped: the render model and wire shape can't
    // silently drift.
    let types::ToolCallUpdate { tool_call_id, fields } = update;
    let update_meta = fields.meta.clone();
    let mut out =
        model::RenderToolCallUpdate::new(tool_call_id, convert_tool_call_update_fields(fields));
    if let Some(meta) = update_meta {
        out = out.meta(meta);
    }
    out
}

pub(super) fn convert_tool_call_update_fields(
    fields: types::ToolCallUpdateFields,
) -> model::RenderToolCallUpdateFields {
    // Exhaustive destructure (no `..`) so a new wire field fails the
    // build until it's mapped. `meta` is lifted onto the model's
    // RenderToolCallUpdate by convert_tool_call_update, not carried here.
    let types::ToolCallUpdateFields {
        title,
        kind,
        status,
        content,
        raw_input,
        raw_output,
        output_metadata,
        task_metadata,
        locations,
        meta: _,
    } = fields;
    let mut out = model::RenderToolCallUpdateFields::new();

    if let Some(title) = title {
        out = out.title(title);
    }
    if let Some(kind) = kind {
        out = out.kind(kind);
    }
    if let Some(status) = status {
        out = out.status(status);
    }
    if let Some(content) = content {
        out = out
            .content(content.into_iter().filter_map(convert_tool_call_content).collect::<Vec<_>>());
    }
    if let Some(raw_input) = raw_input {
        out = out.raw_input(raw_input);
    }
    if let Some(raw_output) = raw_output {
        out = out.raw_output(serde_json::Value::String(raw_output));
    }
    if let Some(output_metadata) = output_metadata {
        out = out.output_metadata(output_metadata);
    }
    if let Some(task_metadata) = task_metadata {
        out = out.task_metadata(task_metadata);
    }
    if let Some(locations) = locations {
        out = out.locations(locations);
    }

    out
}

fn convert_tool_call_content(
    tool_content: types::ToolCallContent,
) -> Option<model::RenderToolCallContent> {
    match tool_content {
        types::ToolCallContent::Content { content } => {
            let block = convert_content_block(content)?;
            Some(model::RenderToolCallContent::Content(model::Content::new(block)))
        }
        types::ToolCallContent::Diff { new_path, old, new, repository } => {
            Some(model::RenderToolCallContent::Diff(
                model::Diff::new(new_path, new).old_text(Some(old)).repository(repository),
            ))
        }
        types::ToolCallContent::McpResource { uri, mime_type, text, blob_saved_to } => {
            Some(model::RenderToolCallContent::McpResource(
                model::McpResource::new(uri)
                    .mime_type(mime_type)
                    .text(text)
                    .blob_saved_to(blob_saved_to),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{convert_tool_call, convert_tool_call_update_fields, map_available_models};
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
                supports_auto_mode: Some(false),
            },
            types::AvailableModel {
                id: "haiku".to_owned(),
                display_name: "Claude Haiku".to_owned(),
                description: None,
                supports_effort: false,
                supported_effort_levels: Vec::new(),
                supports_adaptive_thinking: None,
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
                    .supports_auto_mode(Some(false)),
                model::AvailableModel::new("haiku", "Claude Haiku")
                    .supports_adaptive_thinking(None)
                    .supports_auto_mode(None),
            ]
        );
    }

    #[test]
    fn convert_tool_call_update_fields_preserves_output_metadata() {
        let fields = convert_tool_call_update_fields(types::ToolCallUpdateFields {
            status: Some(types::ToolCallStatus::Completed),
            output_metadata: Some(types::ToolOutputMetadata {
                bash: Some(types::BashOutputMetadata { assistant_auto_backgrounded: Some(true) }),
            }),
            ..types::ToolCallUpdateFields::default()
        });

        assert_eq!(
            fields.output_metadata,
            Some(model::ToolOutputMetadata::new().bash(Some(
                model::BashOutputMetadata::new().assistant_auto_backgrounded(Some(true)),
            )),)
        );
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
            kind: types::ToolKind::Think,
            status: types::ToolCallStatus::Killed,
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
            kind: types::ToolKind::Edit,
            status: types::ToolCallStatus::Completed,
            content: vec![types::ToolCallContent::Diff {
                new_path: "src/main.rs".to_owned(),
                old: "old".to_owned(),
                new: "new".to_owned(),
                repository: Some("stargate/project".to_owned()),
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
            vec![model::RenderToolCallContent::Diff(
                model::Diff::new("src/main.rs", "new")
                    .old_text(Some("old"))
                    .repository(Some("stargate/project".to_owned())),
            )]
        );
    }

    #[test]
    fn convert_tool_call_preserves_mcp_resource_blob_path() {
        let tool_call = convert_tool_call(types::ToolCall {
            tool_call_id: "tool-2".to_owned(),
            title: "ReadMcpResource docs file://manual.pdf".to_owned(),
            kind: types::ToolKind::Read,
            status: types::ToolCallStatus::Completed,
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
            vec![model::RenderToolCallContent::McpResource(
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
