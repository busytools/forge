//! `AskUserQuestion` sequential sequencing. Mirrors upstream's
//! `agent-sdk/src/bridge/user_interaction.ts` (323 `LoC`) - focused on
//! the multi-question loop + answer-payload assembly.
//!
//! The actual loop driver lives in `forge_sdk_worker::run_ask_user_question`
//! because it needs the worker's `pending` map + `event_tx` + the
//! oneshot machinery. This module owns the pure parsing helpers.

use serde_json::{Map, Value};

use forge_primitives::{
    QuestionAnnotation, QuestionOption as TuiQuestionOption, QuestionPrompt, QuestionRequest,
    ToolCall,
};

pub const ASK_USER_QUESTION_TOOL_NAME: &str = "AskUserQuestion";

/// #273: CLI 2.1.156 convention for marking the "best" choice in an
/// `AskUserQuestion` option list is a literal ` (Recommended)`
/// suffix on the option label (space + paren + capital R). When the
/// suffix is present, return the stripped label + `true`; otherwise
/// return the unchanged label + `false`. Match is case-sensitive
/// because the CLI emits the canonical form; lower-case `recommended`
/// from the model's own prose stays as a literal label.
fn strip_recommended_suffix(label: &str) -> (String, bool) {
    const SUFFIX: &str = " (Recommended)";
    if let Some(prefix) = label.strip_suffix(SUFFIX) {
        (prefix.trim_end().to_owned(), true)
    } else {
        (label.to_owned(), false)
    }
}

#[derive(Debug)]
pub struct AskUserQuestionOption {
    pub label: String,
    pub description: String,
    pub preview: Option<String>,
    /// #273: Set when the CLI 2.1.156 wire label carried a
    /// trailing ` (Recommended)` suffix. The suffix is stripped
    /// from `label` so renderers don't have to handle it; the
    /// renderer (or `PromptState::from_question`) bolds and
    /// pre-selects the first recommended option.
    pub recommended: bool,
}

#[derive(Debug)]
pub struct AskUserQuestionPrompt {
    /// Trimmed text; what the TUI displays.
    pub question: String,
    /// The untrimmed wire text. The CLI looks answers and annotations
    /// up by this exact string, so this is the only valid answers-map
    /// key.
    pub question_key: String,
    pub header: String,
    pub multi_select: bool,
    pub options: Vec<AskUserQuestionOption>,
}

/// Mirrors upstream's `parseAskUserQuestionPrompts`. Walks the raw
/// `input.questions` JSON array, dropping malformed entries and
/// requiring at least 2 options per question (matches upstream's
/// validity rule).
pub fn parse_ask_user_question_prompts(input: &Value) -> Vec<AskUserQuestionPrompt> {
    let Some(questions) =
        input.as_object().and_then(|r| r.get("questions")).and_then(Value::as_array)
    else {
        return Vec::new();
    };
    let mut prompts: Vec<AskUserQuestionPrompt> = Vec::new();
    for raw in questions {
        let Some(q) = raw.as_object() else { continue };
        let question_key = q.get("question").and_then(Value::as_str).unwrap_or("").to_owned();
        let question = question_key.trim().to_owned();
        if question.is_empty() {
            continue;
        }
        let header_raw = q.get("header").and_then(Value::as_str).unwrap_or("").trim().to_owned();
        let header =
            if header_raw.is_empty() { format!("Q{}", prompts.len() + 1) } else { header_raw };
        let multi_select = q
            .get("multiSelect")
            .or_else(|| q.get("multi_select"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let mut options: Vec<AskUserQuestionOption> = Vec::new();
        if let Some(opts) = q.get("options").and_then(Value::as_array) {
            for raw_opt in opts {
                let Some(opt) = raw_opt.as_object() else {
                    continue;
                };
                let raw_label =
                    opt.get("label").and_then(Value::as_str).unwrap_or("").trim().to_owned();
                let description =
                    opt.get("description").and_then(Value::as_str).unwrap_or("").trim().to_owned();
                let preview = opt
                    .get("preview")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned);
                let (label, recommended) = strip_recommended_suffix(&raw_label);
                if label.is_empty() {
                    continue;
                }
                options.push(AskUserQuestionOption { label, description, preview, recommended });
            }
        }
        if options.len() < 2 {
            continue;
        }
        prompts.push(AskUserQuestionPrompt {
            question,
            question_key,
            header,
            multi_select,
            options,
        });
    }
    prompts
}

/// Mirrors `askUserQuestionOptions(prompt)` - synthesises wire
/// `option_id` slugs as `question_<index>` so the TUI can map back
/// to the upstream label list when responding.
fn ask_user_question_wire_options(prompt: &AskUserQuestionPrompt) -> Vec<TuiQuestionOption> {
    prompt
        .options
        .iter()
        .enumerate()
        .map(|(i, opt)| TuiQuestionOption {
            option_id: format!("question_{i}"),
            label: opt.label.clone(),
            description: if opt.description.is_empty() {
                None
            } else {
                Some(opt.description.clone())
            },
            preview: opt.preview.clone(),
            recommended: opt.recommended,
        })
        .collect()
}

/// Mirrors `buildQuestionRequest(promptToolCall, prompt, index, total)`.
pub fn build_question_request(
    base_tool_call: &ToolCall,
    prompt: &AskUserQuestionPrompt,
    index: u64,
    total: u64,
) -> QuestionRequest {
    let options = ask_user_question_wire_options(prompt);
    let prompt_tool_call = ToolCall {
        title: prompt.question.clone(),
        raw_input: Some(serde_json::json!({
            "prompt": {
                "question": prompt.question,
                "header": prompt.header,
                "multi_select": prompt.multi_select,
                "options": options,
            },
            "question_index": index,
            "total_questions": total,
        })),
        ..base_tool_call.clone()
    };
    QuestionRequest {
        tool_call: prompt_tool_call,
        prompt: QuestionPrompt {
            question: prompt.question.clone(),
            header: prompt.header.clone(),
            multi_select: prompt.multi_select,
            options,
        },
        question_index: index,
        total_questions: total,
    }
}

/// Mirrors `deriveAnnotation`. Joins option previews with a blank
/// line separator when no caller-supplied annotation preview is
/// present.
pub fn derive_annotation(
    selected: &[TuiQuestionOption],
    incoming: Option<&QuestionAnnotation>,
) -> Option<QuestionAnnotation> {
    let preview_raw = incoming.and_then(|a| a.preview.as_deref()).map_or("", str::trim);
    let preview = if preview_raw.is_empty() {
        let parts: Vec<&str> = selected
            .iter()
            .filter_map(|o| o.preview.as_deref().map(str::trim).filter(|s| !s.is_empty()))
            .collect();
        if parts.is_empty() { String::new() } else { parts.join("\n\n") }
    } else {
        preview_raw.to_owned()
    };
    let notes = incoming
        .and_then(|a| a.notes.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    if preview.is_empty() && notes.is_none() {
        return None;
    }
    Some(QuestionAnnotation {
        preview: if preview.is_empty() { None } else { Some(preview) },
        notes,
    })
}

/// Helper used by the worker's loop driver: build the final
/// `updatedInput` payload after every question has been answered.
/// Mirrors the tail of `requestAskUserQuestionAnswers` upstream.
pub fn build_updated_input(
    original_input: &Value,
    answers: Map<String, Value>,
    annotations: Map<String, Value>,
) -> Value {
    let mut merged = original_input.as_object().cloned().unwrap_or_default();
    merged.insert("answers".to_owned(), Value::Object(answers));
    if !annotations.is_empty() {
        merged.insert("annotations".to_owned(), Value::Object(annotations));
    }
    Value::Object(merged)
}

// ----------------------------------------------------------------
// #273: CLI 2.1.156 Monitor + Workflow typed tool_use inputs.
//
// Placement follows the AskUserQuestion convention above - agent-layer
// types co-located with their parsers. The renderer consumes these
// via the standard `raw_input: Value` -> parse path; the typed
// structs give Tasks 8/9 a clean shape to mutate `UiSession.monitors`
// + `UiSession.workflows` from.
// ----------------------------------------------------------------

/// `Monitor` tool's `tool_use.input` payload. The renderer consumes
/// `description` for the chat one-liner + the MONITORS-section
/// header; `command` is informational; `persistent` toggles the
/// "(persistent)" suffix on the chat notice; `timeout_ms` is
/// honoured by the CLI's task-lifecycle handler.
#[derive(Debug, PartialEq, Eq)]
pub struct MonitorInput {
    pub description: String,
    pub command: String,
    pub persistent: bool,
    pub timeout_ms: u64,
}

/// Parse a `Monitor` tool_use's `raw_input` into a `MonitorInput`.
/// Returns `None` when `description` or `command` is missing or
/// non-string - both are required for a meaningful render.
pub fn parse_monitor_input(input: &Value) -> Option<MonitorInput> {
    let obj = input.as_object()?;
    let description = obj.get("description").and_then(Value::as_str)?.trim().to_owned();
    let command = obj.get("command").and_then(Value::as_str)?.trim().to_owned();
    if description.is_empty() || command.is_empty() {
        return None;
    }
    let persistent = obj.get("persistent").and_then(Value::as_bool).unwrap_or(false);
    // Wire field is `timeout_ms` per plan; default to 0 (which the
    // renderer reads as "no explicit timeout, persistent or
    // CLI-default").
    let timeout_ms = obj.get("timeout_ms").and_then(Value::as_u64).unwrap_or(0);
    Some(MonitorInput { description, command, persistent, timeout_ms })
}

/// `Workflow` tool's `tool_use.input` payload. The CLI parses +
/// executes the JS source itself; forge preserves the script
/// verbatim for the WORKFLOWS-section header (`meta` block is
/// extracted via substring at render time in Task 9).
#[derive(Debug, PartialEq, Eq)]
pub struct WorkflowInput {
    pub script: String,
}

/// Parse a `Workflow` tool_use's `raw_input` into a `WorkflowInput`.
/// Returns `None` when `script` is missing or non-string - the
/// renderer can't show a phase tree or even an inferred meta name
/// without the source.
pub fn parse_workflow_input(input: &Value) -> Option<WorkflowInput> {
    let script = input.as_object()?.get("script").and_then(Value::as_str)?.to_owned();
    if script.is_empty() {
        return None;
    }
    Some(WorkflowInput { script })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_drops_invalid_entries() {
        let input = json!({"questions": [
            { "question": "  ", "options": [{"label":"a"},{"label":"b"}] },
            { "question": "Q1", "options": [{"label":"only"}] },
            { "question": "Q2", "header": "H2", "multiSelect": true, "options": [
                {"label":"L1","description":"d1"},
                {"label":"L2","description":"","preview":"p2"},
            ]},
            "not_an_object",
        ]});
        let prompts = parse_ask_user_question_prompts(&input);
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0].question, "Q2");
        assert_eq!(prompts[0].header, "H2");
        assert!(prompts[0].multi_select);
        assert_eq!(prompts[0].options.len(), 2);
        assert_eq!(prompts[0].options[1].preview.as_deref(), Some("p2"));
    }

    #[test]
    fn empty_questions_yields_empty_prompts() {
        assert!(parse_ask_user_question_prompts(&json!({})).is_empty());
        assert!(parse_ask_user_question_prompts(&json!({"questions": []})).is_empty());
    }

    #[test]
    fn header_falls_back_to_q_n_when_blank() {
        let input = json!({"questions": [{
            "question": "first",
            "options": [{"label":"a"},{"label":"b"}],
        }, {
            "question": "second",
            "options": [{"label":"a"},{"label":"b"}],
        }]});
        let prompts = parse_ask_user_question_prompts(&input);
        assert_eq!(prompts[0].header, "Q1");
        assert_eq!(prompts[1].header, "Q2");
    }

    #[test]
    fn build_request_carries_index_and_total() {
        let prompt = AskUserQuestionPrompt {
            question: "Q?".to_owned(),
            question_key: "Q?".to_owned(),
            header: "H".to_owned(),
            multi_select: false,
            options: vec![
                AskUserQuestionOption {
                    label: "A".to_owned(),
                    description: String::new(),
                    preview: None,
                    recommended: false,
                },
                AskUserQuestionOption {
                    label: "B".to_owned(),
                    description: "desc".to_owned(),
                    preview: Some("p".to_owned()),
                    recommended: false,
                },
            ],
        };
        let base = ToolCall {
            tool_call_id: "tu".to_owned(),
            title: "AskUserQuestion".to_owned(),
            kind: forge_primitives::ToolKind::Other,
            status: forge_primitives::ToolCallStatus::Pending,
            content: Vec::new(),
            raw_input: None,
            raw_output: None,
            output_metadata: None,
            task_metadata: None,
            locations: Vec::new(),
            meta: None,
        };
        let req = build_question_request(&base, &prompt, 1, 3);
        assert_eq!(req.question_index, 1);
        assert_eq!(req.total_questions, 3);
        assert_eq!(req.prompt.question, "Q?");
        assert_eq!(req.prompt.options.len(), 2);
        assert_eq!(req.prompt.options[0].option_id, "question_0");
        assert_eq!(req.prompt.options[1].option_id, "question_1");
        assert_eq!(req.prompt.options[1].preview.as_deref(), Some("p"));
        assert_eq!(req.tool_call.title, "Q?");
    }

    #[test]
    fn derive_annotation_joins_previews_when_caller_omitted() {
        let opts = vec![
            TuiQuestionOption {
                option_id: "question_0".to_owned(),
                label: "A".to_owned(),
                description: None,
                preview: Some("a-preview".to_owned()),
                recommended: false,
            },
            TuiQuestionOption {
                option_id: "question_1".to_owned(),
                label: "B".to_owned(),
                description: None,
                preview: Some("b-preview".to_owned()),
                recommended: false,
            },
        ];
        let derived = derive_annotation(&opts, None).unwrap();
        assert_eq!(derived.preview.as_deref(), Some("a-preview\n\nb-preview"));
        assert!(derived.notes.is_none());
    }

    // ----------------------------------------------------------------
    // #273: (Recommended) suffix detection.
    // ----------------------------------------------------------------

    #[test]
    fn parse_detects_recommended_suffix_and_strips_label() {
        let input = json!({"questions": [{
            "question": "Pick a rule shape",
            "options": [
                {"label": "Use deny rules (Recommended)"},
                {"label": "Use allow rules"},
            ],
        }]});
        let prompts = parse_ask_user_question_prompts(&input);
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0].options.len(), 2);
        assert_eq!(prompts[0].options[0].label, "Use deny rules");
        assert!(prompts[0].options[0].recommended);
        assert_eq!(prompts[0].options[1].label, "Use allow rules");
        assert!(!prompts[0].options[1].recommended);
    }

    #[test]
    fn parse_recommended_suffix_is_case_sensitive() {
        // Lowercase / mid-string `recommended` stays as a literal
        // part of the label - CLI emits the canonical
        // ` (Recommended)` form only.
        let input = json!({"questions": [{
            "question": "Q",
            "options": [
                {"label": "Plan with rationale (recommended)"},
                {"label": "Quick reply"},
            ],
        }]});
        let prompts = parse_ask_user_question_prompts(&input);
        assert_eq!(prompts[0].options[0].label, "Plan with rationale (recommended)");
        assert!(!prompts[0].options[0].recommended);
    }

    #[test]
    fn build_request_propagates_recommended_flag_to_wire_option() {
        let prompt = AskUserQuestionPrompt {
            question: "Q?".to_owned(),
            question_key: "Q?".to_owned(),
            header: "H".to_owned(),
            multi_select: false,
            options: vec![
                AskUserQuestionOption {
                    label: "A".to_owned(),
                    description: String::new(),
                    preview: None,
                    recommended: false,
                },
                AskUserQuestionOption {
                    label: "B".to_owned(),
                    description: String::new(),
                    preview: None,
                    recommended: true,
                },
            ],
        };
        let base = ToolCall {
            tool_call_id: "tu".to_owned(),
            title: "AskUserQuestion".to_owned(),
            kind: forge_primitives::ToolKind::Other,
            status: forge_primitives::ToolCallStatus::Pending,
            content: Vec::new(),
            raw_input: None,
            raw_output: None,
            output_metadata: None,
            task_metadata: None,
            locations: Vec::new(),
            meta: None,
        };
        let req = build_question_request(&base, &prompt, 0, 1);
        assert!(!req.prompt.options[0].recommended);
        assert!(req.prompt.options[1].recommended);
    }

    #[test]
    fn derive_annotation_falls_through_when_nothing_to_say() {
        let opts: Vec<TuiQuestionOption> = Vec::new();
        assert!(derive_annotation(&opts, None).is_none());
    }

    // ----------------------------------------------------------------
    // #273: Monitor + Workflow typed input parsers.
    // ----------------------------------------------------------------

    #[test]
    fn parse_monitor_input_reads_required_and_optional_fields() {
        let input = json!({
            "description": "watch redis",
            "command": "redis-cli monitor",
            "persistent": true,
            "timeout_ms": 300_000,
        });
        let parsed = parse_monitor_input(&input).expect("valid input");
        assert_eq!(parsed.description, "watch redis");
        assert_eq!(parsed.command, "redis-cli monitor");
        assert!(parsed.persistent);
        assert_eq!(parsed.timeout_ms, 300_000);
    }

    #[test]
    fn parse_monitor_input_defaults_persistent_and_timeout() {
        let input = json!({"description": "logs", "command": "tail -F app.log"});
        let parsed = parse_monitor_input(&input).expect("valid input");
        assert!(!parsed.persistent, "persistent defaults to false");
        assert_eq!(parsed.timeout_ms, 0, "timeout_ms defaults to 0");
    }

    #[test]
    fn parse_monitor_input_returns_none_when_required_fields_missing() {
        for malformed in [
            json!({}),
            json!({"description": "x"}),
            json!({"command": "y"}),
            json!({"description": "", "command": "y"}),
            json!({"description": "x", "command": ""}),
            json!({"description": 42, "command": "y"}),
            json!(["not", "an", "object"]),
        ] {
            assert!(parse_monitor_input(&malformed).is_none(), "expected None for {malformed:?}");
        }
    }

    #[test]
    fn parse_workflow_input_preserves_script_verbatim() {
        let script = "export const meta = { name: 'minimal-ping' }\nphase('Ping')";
        let input = json!({"script": script});
        let parsed = parse_workflow_input(&input).expect("valid input");
        assert_eq!(parsed.script, script);
    }

    #[test]
    fn parse_workflow_input_returns_none_when_script_missing_or_empty() {
        for malformed in
            [json!({}), json!({"script": ""}), json!({"script": 42}), json!({"other": "x"})]
        {
            assert!(parse_workflow_input(&malformed).is_none(), "expected None for {malformed:?}");
        }
    }
}
