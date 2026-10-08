//! System One MCP - ask the configured decision model one typed
//! question (`mcp__forge__systemone__*`).
//!
//! One tool per primitive, each asking exactly one question about one
//! state; several questions about one state are several parallel calls.
//! All three are any-caller, and they are injected ONLY when
//! `[systemone]` is present and enabled - a disabled feature leaves no
//! tool to call.
//!
//! - [`facade`] - the `SystemOneFacade` seam (prod over the boot-built
//!   client + a mock for tool tests).

use std::sync::Arc;

use forge_sdk::mcp::server::McpServerBuilder;
use forge_sdk::mcp::tool::{Tool, ToolInput, ToolOutput};
use forge_system_one::{AskOutcome, NoulCriteria, Question, SystemOneError};

use crate::mcp::systemone::facade::SystemOneFacade;
use crate::mcp::systemone::types::{ChoiceArgs, NoulArgs, ScoreArgs};

pub(crate) mod facade;
pub(crate) mod types;

/// Attach the three systemone tools; called only when a client exists,
/// for both session kinds (any-caller).
pub(crate) fn add_tools(
    builder: McpServerBuilder,
    facade: Arc<dyn SystemOneFacade>,
) -> McpServerBuilder {
    let noul = AskNoul { facade: Arc::clone(&facade) };
    let choice = AskChoice { facade: Arc::clone(&facade) };
    let score = AskScore { facade };
    builder.tool(noul).tool(choice).tool(score)
}

fn tool_error(text: String) -> ToolOutput {
    ToolOutput::error(text)
}

/// The tool-output shape: what answered, the typed answer, and what it
/// cost when the provider reported it.
fn outcome_to_text(outcome: &AskOutcome) -> String {
    let mut value = serde_json::json!({
        "model": outcome.model,
        "answer": outcome.answer,
    });
    if let Some(usage) = &outcome.usage {
        value["usage"] = serde_json::json!(usage);
    }
    value.to_string()
}

fn format_ask_error(err: &SystemOneError) -> String {
    match err {
        SystemOneError::Http { status: 401, .. } => {
            "System One rejected the API key (HTTP 401); check `api_key` in forge.toml [systemone]".to_owned()
        }
        SystemOneError::Http { status, body } => format!("System One returned HTTP {status}: {body}"),
        SystemOneError::Timeout => {
            "System One request timed out (raise `timeout_ms` in forge.toml [systemone] if the state is large)".to_owned()
        }
        SystemOneError::Transport(message) => format!("System One request failed: {message}"),
        SystemOneError::InvalidResponse(detail) => format!("System One returned an invalid answer: {detail}"),
    }
}

fn noul_question(args: &NoulArgs) -> Result<Question, String> {
    let criteria = match &args.criteria {
        Some(criteria) => {
            let mut criteria = criteria.clone();
            let true_value = criteria.remove("true");
            let false_value = criteria.remove("false");
            match (true_value, false_value, criteria.is_empty()) {
                (Some(true_value), Some(false_value), true) => {
                    Some(NoulCriteria { r#true: true_value, r#false: false_value })
                }
                _ => {
                    return Err(
                        "noul criteria needs exactly the keys `true` and `false`".to_owned()
                    );
                }
            }
        }
        None => None,
    };
    Ok(Question::Noul { instructions: args.instructions.clone(), criteria })
}

fn choice_question(args: &ChoiceArgs) -> Result<Question, String> {
    if args.criteria.is_empty() {
        return Err("choice criteria needs at least one option".to_owned());
    }
    if args.criteria.len() > 255 {
        return Err(format!("choice criteria has {} options (max 255)", args.criteria.len()));
    }
    Ok(Question::Choice {
        instructions: args.instructions.clone(),
        criteria: args.criteria.clone(),
    })
}

fn score_question(args: &ScoreArgs) -> Result<Question, String> {
    match args.criteria.len() {
        0 | 1 => {
            return Err(format!(
                "score criteria needs at least 2 levels (got {})",
                args.criteria.len()
            ));
        }
        count if count > 10 => return Err(format!("score criteria has {count} levels (max 10)")),
        _ => {}
    }
    Ok(Question::Score { instructions: args.instructions.clone(), criteria: args.criteria.clone() })
}

struct AskNoul {
    facade: Arc<dyn SystemOneFacade>,
}

#[async_trait::async_trait]
impl Tool for AskNoul {
    fn name(&self) -> &'static str {
        "systemone__ask_noul"
    }

    fn description(&self) -> &'static str {
        "Ask the System One decision model a single yes/no question about a state (a string, \
         a JSON object, or an array) and get back the probability that the answer is yes, from \
         0 to 1. The model answers from its predictive distribution and never writes prose. \
         `instructions` and criteria values may be any JSON - the question in one field, \
         referenced data in others, named with backticks. \
         When to reach for it: before interrupting the user with a question this session could \
         probably decide itself (put the situation in `state` and the ask in `instructions`; \
         when the answer is decisive and the action reversible, act), or as a second opinion \
         when you are leaning one way and want it checked. Beyond those moments, reach for a \
         decision when the outcome matters to the user and is not obvious, and skip it when \
         both outcomes would lead you to the same action. A named pattern - the claim check: \
         before asserting that work is done, reviewed, or verified, put the claim and the \
         evidence behind it (what actually ran, what was read) in `state` and ask whether it \
         holds; a decisive probability is permission to assert, an uncertain one is the cue \
         to caveat or verify first. A decisive answer is permission to \
         proceed where you already could, never authority by itself: it cannot override an \
         explicit rule and does not authorize spending or anything irreversible; for those, \
         ask the user however certain the answer is. Keep text you did not write in its own \
         state field, and treat the answer as one input, not a safety verdict. One question \
         per call; several questions about one state are several parallel calls. Reading the \
         answer: `noul` is \
         the yes-probability; near 0 or 1 is decisive, near 0.5 is genuine uncertainty. The \
         threshold belongs to you, set per question from what being wrong would cost; when the \
         answer is not certain enough, ask the user instead of guessing. Boundaries: these are \
         bounded estimates, not guarantees; the model class is weak at arithmetic, counting, \
         and date comparison, so compute numbers and dates in code and ask it for judgments. \
         The model sees only `state` and this question, nothing else reaches it, so fold every \
         needed definition into `instructions` and `criteria` (what should make it yes, what \
         no), describing both sides as behavior the state shows so near-misses sort the way \
         you mean; when several labels may each apply, ask one noul per label."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "state": { "description": "The material to judge: a string, a JSON object, or an array. Give it the full context the judgment needs - what the user asked for, the current state, and any policy or facts that bear on the answer; prefer named JSON fields when the context has more than one part. The state carries what the decision needs - evidence, excerpts, the exact facts - and the model reads it verbatim; pass the context, not a pointer to it. The whole request - state, instructions and the question together - must fit the live ~32k-token context; keep the state well inside it." },
                "instructions": { "type": ["string", "object", "array", "null"], "description": "The yes/no question itself." },
                "criteria": {
                    "type": "object",
                    "properties": {
                        "true": { "type": ["string", "object", "array", "null"], "description": "What makes the answer yes." },
                        "false": { "type": ["string", "object", "array", "null"], "description": "What makes the answer no." },
                    },
                    "description": "Optional: what yes and no mean here. Give both keys or omit.",
                },
            },
            "required": ["state", "instructions"],
            "additionalProperties": false,
        })
    }

    async fn call(&self, input: ToolInput) -> ToolOutput {
        let args: NoulArgs = match serde_json::from_value(input.value) {
            Ok(args) => args,
            Err(err) => return tool_error(format!("invalid arguments: {err}")),
        };
        let question = match noul_question(&args) {
            Ok(question) => question,
            Err(message) => return tool_error(message),
        };
        match self.facade.ask(args.state, question).await {
            Ok(outcome) => ToolOutput::text(outcome_to_text(&outcome)),
            Err(err) => tool_error(format_ask_error(&err)),
        }
    }
}

struct AskChoice {
    facade: Arc<dyn SystemOneFacade>,
}

#[async_trait::async_trait]
impl Tool for AskChoice {
    fn name(&self) -> &'static str {
        "systemone__ask_choice"
    }

    fn description(&self) -> &'static str {
        "Ask the System One decision model to choose one option from the set you define, for a \
         state (a string, a JSON object, or an array). Returns the winning option, the \
         probability of every option, and a confidence for the distribution. The model never \
         writes prose. `instructions` and criteria values may be any JSON - the question in \
         one field, referenced data in others, named with backticks. \
         When to reach for it: routing and picking between enumerated \
         alternatives, or as a second opinion when you are leaning toward one option and want \
         the alternatives weighed. Beyond those moments, reach for a decision when the outcome \
         matters to the user and is not obvious, and skip it when both outcomes would lead you \
         to the same action; a routing call with several plausible owners and no evidence \
         separating them is exactly that case. List every option in `criteria`. A null \
         value is fine when the name stands alone; add a \"when this applies\" clause only \
         where a subtle distinction needs naming, because nulls are the common case and \
         six clauses for a five-way choice \
         cost more than the choice returns (at most 255 options); when nothing may fit the \
         state, include an explicit no-match option, because the model can only choose among \
         the options you list. Reading \
         the answer: `choice` is the highest-probability option; read `probabilities` for the \
         full distribution, and treat a close runner-up as uncertainty rather than a decision; \
         escalate to the user when the margin does not clear what the decision costs. \
         `confidence` is a statistic of the distribution, not a calibrated chance of being \
         right. Boundaries: one question per call; the model sees only `state` and the options \
         you list, nothing else reaches it; probabilities are estimates, not guarantees; the \
         model can favour whichever option is listed first, so when an answer matters, re-ask \
         with the options reordered and check that it holds."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "state": { "description": "The material to judge: a string, a JSON object, or an array. Give it the full context the judgment needs - what the user asked for, the current state, and any policy or facts that bear on the answer; prefer named JSON fields when the context has more than one part. The state carries what the decision needs - evidence, excerpts, the exact facts - and the model reads it verbatim; pass the context, not a pointer to it. The whole request - state, instructions and the question together - must fit the live ~32k-token context; keep the state well inside it." },
                "instructions": { "type": ["string", "object", "array", "null"], "description": "The question the options answer." },
                "criteria": {
                    "type": "object",
                    "additionalProperties": { "type": ["string", "object", "array", "null"] },
                    "description": "Option names mapped to when each applies (null when the name stands alone). Every option must be listed.",
                },
            },
            "required": ["state", "instructions", "criteria"],
            "additionalProperties": false,
        })
    }

    async fn call(&self, input: ToolInput) -> ToolOutput {
        let args: ChoiceArgs = match serde_json::from_value(input.value) {
            Ok(args) => args,
            Err(err) => return tool_error(format!("invalid arguments: {err}")),
        };
        let question = match choice_question(&args) {
            Ok(question) => question,
            Err(message) => return tool_error(message),
        };
        match self.facade.ask(args.state, question).await {
            Ok(outcome) => ToolOutput::text(outcome_to_text(&outcome)),
            Err(err) => tool_error(format_ask_error(&err)),
        }
    }
}

struct AskScore {
    facade: Arc<dyn SystemOneFacade>,
}

#[async_trait::async_trait]
impl Tool for AskScore {
    fn name(&self) -> &'static str {
        "systemone__ask_score"
    }

    fn description(&self) -> &'static str {
        "Ask the System One decision model to place a state (a string, a JSON object, or an \
         array) on an ordered rubric you define. Returns a fractional score (it can land \
         between levels), the probability of each level, a legend mapping level positions to \
         your text, and a confidence. The model never writes prose. \
         `instructions` and criteria values may be any JSON - the question in one field, \
         referenced data in others, named with backticks. \
         When to reach for it: \
         severity, quality, priority, or risk judgments where the levels are meaningful to you. \
         Beyond those moments, score a judgment when it matters to the user and is not obvious, \
         and skip it when the position would not change what you do. \
         Define `criteria` as an ordered list of level descriptions, lowest first (two to ten \
         levels); each level should describe a concrete situation and stand on its own. Reading \
         the answer: `score` is the probability-weighted position (for example 1.79 on a \
         three-level rubric); `probabilities` shows how settled the answer is; `confidence` is \
         a statistic of the distribution, not a calibrated chance of being right; the threshold \
         belongs to you, set from what being wrong would cost; escalate to the user when the \
         position is not settled enough for the stakes. Boundaries: each level \
         description is a judgment anchor, not a unit of measure, so do not read gaps between \
         scores as magnitudes; the model sees only `state` and this rubric, nothing else \
         reaches it, so fold every needed definition into `instructions` and `criteria`; the \
         model class is weak at arithmetic and date comparison, so compute numbers in code. \
         One question per call."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "state": { "description": "The material to judge: a string, a JSON object, or an array. Give it the full context the judgment needs - what the user asked for, the current state, and any policy or facts that bear on the answer; prefer named JSON fields when the context has more than one part. The state carries what the decision needs - evidence, excerpts, the exact facts - and the model reads it verbatim; pass the context, not a pointer to it. The whole request - state, instructions and the question together - must fit the live ~32k-token context; keep the state well inside it." },
                "instructions": { "type": ["string", "object", "array", "null"], "description": "The rubric question itself." },
                "criteria": {
                    "type": "array",
                    "items": { "type": ["string", "object", "array"] },
                    "minItems": 2,
                    "maxItems": 10,
                    "description": "The ordered level descriptions, lowest first.",
                },
            },
            "required": ["state", "instructions", "criteria"],
            "additionalProperties": false,
        })
    }

    async fn call(&self, input: ToolInput) -> ToolOutput {
        let args: ScoreArgs = match serde_json::from_value(input.value) {
            Ok(args) => args,
            Err(err) => return tool_error(format!("invalid arguments: {err}")),
        };
        let question = match score_question(&args) {
            Ok(question) => question,
            Err(message) => return tool_error(message),
        };
        match self.facade.ask(args.state, question).await {
            Ok(outcome) => ToolOutput::text(outcome_to_text(&outcome)),
            Err(err) => tool_error(format_ask_error(&err)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::systemone::facade::MockSystemOneFacade;
    use crate::mcp::test_support::text_of;
    use forge_system_one::{Answer, Usage};

    fn input(value: serde_json::Value) -> ToolInput {
        ToolInput { value }
    }

    fn outcome(answer: Answer) -> AskOutcome {
        AskOutcome {
            model: "test-model".to_owned(),
            answer,
            usage: Some(Usage { input_tokens: 10, output_tokens: 3, cost: None }),
        }
    }

    /// The tool text ships to every caller that has the tools, so the two
    /// sentences that teach the shape a call takes and the budget it must
    /// fit are pinned here: a silent delete fails instead of shipping.
    #[test]
    fn the_tool_text_carries_its_structured_values_and_budget_sentences() {
        let noul = AskNoul { facade: MockSystemOneFacade::new().into_arc() };
        let choice = AskChoice { facade: MockSystemOneFacade::new().into_arc() };
        let score = AskScore { facade: MockSystemOneFacade::new().into_arc() };
        for (tool, description, schema) in [
            ("systemone__ask_noul", noul.description(), noul.input_schema()),
            ("systemone__ask_choice", choice.description(), choice.input_schema()),
            ("systemone__ask_score", score.description(), score.input_schema()),
        ] {
            assert!(
                description.contains(
                    "`instructions` and criteria values may be any JSON - the question in one \
                     field, referenced data in others, named with backticks"
                ),
                "{tool} must teach the structured values it takes: {description}"
            );
            let state = schema["properties"]["state"]["description"]
                .as_str()
                .expect("the state description is a string");
            for clause in [
                "The state carries what the decision needs",
                "pass the context, not a pointer to it",
                "must fit the live ~32k-token context; keep the state well inside it",
            ] {
                assert!(
                    state.contains(clause),
                    "{tool}'s state description owes {clause:?}: {state}"
                );
            }
        }
    }

    /// The choice text names its routing moment by the test that decides it:
    /// no evidence separating the owners. Pinned so the vaguer trigger it
    /// replaced cannot come back.
    #[test]
    fn the_choice_description_names_its_routing_evidence_test() {
        let choice = AskChoice { facade: MockSystemOneFacade::new().into_arc() };
        assert!(
            choice.description().contains(
                "a routing call with several plausible owners and no evidence separating them"
            ),
            "the routing moment's evidence test is pinned: {}",
            choice.description()
        );
    }

    #[tokio::test]
    async fn ask_noul_omits_absent_usage_from_the_output() {
        let mock = Arc::new(MockSystemOneFacade::new());
        *mock.result.lock() = Some(Ok(AskOutcome {
            model: "m".to_owned(),
            answer: Answer::Noul { noul: 0.5 },
            usage: None,
        }));
        let tool = AskNoul { facade: mock.clone() };

        let out = tool.call(input(serde_json::json!({"state": "x", "instructions": "y"}))).await;

        assert!(
            !out.is_error,
            "a usage-less answer is still a delivered decision: {}",
            text_of(&out)
        );
        assert!(!text_of(&out).contains("usage"), "no invented usage block: {}", text_of(&out));
        assert!(text_of(&out).contains("\"noul\":0.5"), "{}", text_of(&out));
    }

    #[tokio::test]
    async fn ask_noul_returns_the_typed_answer_and_reaches_the_facade() {
        let mock = Arc::new(MockSystemOneFacade::new());
        *mock.result.lock() = Some(Ok(outcome(Answer::Noul { noul: 0.83 })));
        let tool = AskNoul { facade: mock.clone() };

        let out = tool
            .call(input(serde_json::json!({
                "state": {"ticket": "x"},
                "instructions": "Is this a billing issue?",
                "criteria": {"true": "Payments", "false": "Anything else"}
            })))
            .await;

        assert!(!out.is_error, "noul succeeds: {}", text_of(&out));
        assert!(text_of(&out).contains("\"noul\":0.83"), "{}", text_of(&out));
        assert!(text_of(&out).contains("test-model"), "{}", text_of(&out));
        let calls = mock.calls.lock();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, serde_json::json!({"ticket": "x"}));
        assert_eq!(
            calls[0].1,
            Question::Noul {
                instructions: serde_json::json!("Is this a billing issue?"),
                criteria: Some(NoulCriteria {
                    r#true: serde_json::json!("Payments"),
                    r#false: serde_json::json!("Anything else")
                }),
            }
        );
    }

    /// The API's own typing: `instructions` and criteria values are any
    /// JSON, and the tool must carry the structure through verbatim -
    /// the pre-flight constraints still fire by name around it.
    #[tokio::test]
    async fn structured_values_reach_the_facade_verbatim() {
        let mock = Arc::new(MockSystemOneFacade::new());
        *mock.result.lock() = Some(Ok(outcome(Answer::Noul { noul: 0.83 })));

        let noul = AskNoul { facade: mock.clone() };
        let out = noul
            .call(input(serde_json::json!({
                "state": {"ticket": "x"},
                "instructions": {"question": "Is this a billing issue?", "policy": "refunds within 30 days"},
                "criteria": {"true": {"rule": "Payments"}, "false": "Anything else"}
            })))
            .await;
        assert!(!out.is_error, "structured noul arguments are accepted: {}", text_of(&out));

        let choice = AskChoice { facade: mock.clone() };
        let out = choice
            .call(input(serde_json::json!({
                "state": "x",
                "instructions": {"question": "Which team?", "touched": ["a.rs"]},
                "criteria": {"billing": {"files": ["a.rs"]}, "frontend": null}
            })))
            .await;
        assert!(!out.is_error, "structured choice values are accepted: {}", text_of(&out));

        let score = AskScore { facade: mock.clone() };
        let out = score
            .call(input(serde_json::json!({
                "state": "x",
                "instructions": {"question": "How urgent?", "note": "per `sev.md`"},
                "criteria": ["Routine", {"label": "Urgent", "scope": "page now"}]
            })))
            .await;
        assert!(!out.is_error, "a structured score question is accepted: {}", text_of(&out));

        let malformed = noul
            .call(input(serde_json::json!({
                "state": "x",
                "instructions": "Is it?",
                "criteria": {"true": {"rule": "yes"}, "false": "no", "maybe": "hmm"}
            })))
            .await;
        assert!(malformed.is_error, "a third noul key is still refused");
        assert!(
            text_of(&malformed).contains("exactly the keys `true` and `false`"),
            "{}",
            text_of(&malformed)
        );

        let calls = mock.calls.lock();
        assert_eq!(calls.len(), 3, "the malformed call never reaches the facade");
        assert_eq!(
            serde_json::to_value(&calls[0].1).expect("question serializes"),
            serde_json::json!({
                "type": "noul",
                "instructions": {"question": "Is this a billing issue?", "policy": "refunds within 30 days"},
                "criteria": {"true": {"rule": "Payments"}, "false": "Anything else"}
            }),
            "structured noul values reach the wire verbatim"
        );
        assert_eq!(
            serde_json::to_value(&calls[1].1).expect("question serializes"),
            serde_json::json!({
                "type": "choice",
                "instructions": {"question": "Which team?", "touched": ["a.rs"]},
                "criteria": {"billing": {"files": ["a.rs"]}, "frontend": null}
            }),
            "structured choice values reach the wire verbatim"
        );
        assert_eq!(
            serde_json::to_value(&calls[2].1).expect("question serializes"),
            serde_json::json!({
                "type": "score",
                "instructions": {"question": "How urgent?", "note": "per `sev.md`"},
                "criteria": ["Routine", {"label": "Urgent", "scope": "page now"}]
            }),
            "structured score levels reach the wire verbatim"
        );
    }

    /// What each tool advertises: the structured union the API accepts,
    /// minus the score tool's levels, which stay strings.
    #[test]
    fn schemas_advertise_the_structured_union() {
        let noul = AskNoul { facade: MockSystemOneFacade::new().into_arc() }.input_schema();
        let choice = AskChoice { facade: MockSystemOneFacade::new().into_arc() }.input_schema();
        let score = AskScore { facade: MockSystemOneFacade::new().into_arc() }.input_schema();

        let union = serde_json::json!(["string", "object", "array", "null"]);
        assert_eq!(
            noul["properties"]["instructions"]["type"], union,
            "noul instructions advertise the API's union"
        );
        assert_eq!(noul["properties"]["criteria"]["properties"]["true"]["type"], union);
        assert_eq!(noul["properties"]["criteria"]["properties"]["false"]["type"], union);
        assert_eq!(
            choice["properties"]["criteria"]["additionalProperties"]["type"], union,
            "choice option values advertise the API's union"
        );
        assert_eq!(
            score["properties"]["criteria"]["items"]["type"],
            serde_json::json!(["string", "object", "array"]),
            "score levels advertise the API's union, which has no null"
        );
        for (tool, schema) in [("noul", &noul), ("choice", &choice), ("score", &score)] {
            assert_eq!(
                schema["properties"]["instructions"]["type"], union,
                "{tool} instructions advertise the API's union"
            );
        }
    }

    #[tokio::test]
    async fn ask_noul_criteria_needs_exactly_true_and_false() {
        let mock = Arc::new(MockSystemOneFacade::new());
        let tool = AskNoul { facade: mock.clone() };

        let out = tool.call(input(serde_json::json!({"state": "x", "instructions": "Is it?", "criteria": {"true": "yes"}}))).await;

        assert!(out.is_error);
        assert!(text_of(&out).contains("exactly the keys `true` and `false`"), "{}", text_of(&out));
        assert!(mock.calls.lock().is_empty(), "malformed criteria never reaches the facade");
    }

    #[tokio::test]
    async fn ask_choice_returns_the_choice_answer() {
        let mock = Arc::new(MockSystemOneFacade::new());
        let answer = Answer::Choice {
            choice: "billing".to_owned(),
            probabilities: Some(
                [("billing".to_owned(), 0.9), ("frontend".to_owned(), 0.1)].into_iter().collect(),
            ),
            confidence: Some(0.8),
        };
        *mock.result.lock() = Some(Ok(outcome(answer)));
        let tool = AskChoice { facade: mock.clone() };

        let out = tool.call(input(serde_json::json!({"state": "x", "instructions": "Which team?", "criteria": {"billing": "Payments", "frontend": null}}))).await;

        assert!(!out.is_error, "choice succeeds: {}", text_of(&out));
        assert!(text_of(&out).contains("\"choice\":\"billing\""), "{}", text_of(&out));
        assert!(text_of(&out).contains("\"probabilities\""), "{}", text_of(&out));
    }

    #[tokio::test]
    async fn ask_choice_rejects_empty_criteria() {
        let mock = Arc::new(MockSystemOneFacade::new());
        let tool = AskChoice { facade: mock.clone() };

        let out = tool
            .call(input(
                serde_json::json!({"state": "x", "instructions": "Which?", "criteria": {}}),
            ))
            .await;

        assert!(out.is_error);
        assert!(text_of(&out).contains("at least one option"), "{}", text_of(&out));
        assert!(mock.calls.lock().is_empty());
    }

    #[tokio::test]
    async fn ask_choice_rejects_too_many_options() {
        let mock = Arc::new(MockSystemOneFacade::new());
        let tool = AskChoice { facade: mock.clone() };
        let criteria: serde_json::Map<String, serde_json::Value> =
            (0..256).map(|index| (format!("o{index}"), serde_json::Value::Null)).collect();

        let out = tool
            .call(input(
                serde_json::json!({"state": "x", "instructions": "Which?", "criteria": criteria}),
            ))
            .await;

        assert!(out.is_error);
        assert!(text_of(&out).contains("256 options (max 255)"), "{}", text_of(&out));
        assert!(mock.calls.lock().is_empty());
    }

    #[tokio::test]
    async fn ask_score_returns_the_rubric_answer() {
        let mock = Arc::new(MockSystemOneFacade::new());
        let answer = Answer::Score {
            score: 1.79,
            probabilities: Some(
                [("0".to_owned(), 0.1), ("1".to_owned(), 0.3), ("2".to_owned(), 0.6)]
                    .into_iter()
                    .collect(),
            ),
            confidence: Some(0.5),
            legend: Some(
                [
                    ("0".to_owned(), "Routine".to_owned()),
                    ("1".to_owned(), "Soon".to_owned()),
                    ("2".to_owned(), "Urgent".to_owned()),
                ]
                .into_iter()
                .collect(),
            ),
        };
        *mock.result.lock() = Some(Ok(outcome(answer)));
        let tool = AskScore { facade: mock.clone() };

        let out = tool
            .call(input(serde_json::json!({"state": "x", "instructions": "How urgent?", "criteria": ["Routine", "Soon", "Urgent"]})))
            .await;

        assert!(!out.is_error, "score succeeds: {}", text_of(&out));
        assert!(text_of(&out).contains("\"score\":1.79"), "{}", text_of(&out));
        assert!(text_of(&out).contains("\"legend\""), "{}", text_of(&out));
        assert!(text_of(&out).contains("\"probabilities\""), "{}", text_of(&out));
        let calls = mock.calls.lock();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].1,
            Question::Score {
                instructions: serde_json::json!("How urgent?"),
                criteria: vec![
                    serde_json::json!("Routine"),
                    serde_json::json!("Soon"),
                    serde_json::json!("Urgent"),
                ],
            }
        );
    }

    #[tokio::test]
    async fn boundary_criteria_are_accepted_and_reach_the_facade() {
        let mock = Arc::new(MockSystemOneFacade::new());
        let choice = AskChoice { facade: mock.clone() };

        let out = choice.call(input(serde_json::json!({"state": "x", "instructions": "Which?", "criteria": {"only": null}}))).await;
        assert!(!out.is_error, "one option is accepted: {}", text_of(&out));

        let wide: serde_json::Map<String, serde_json::Value> =
            (0..255).map(|index| (format!("o{index}"), serde_json::Value::Null)).collect();
        let out = choice
            .call(input(
                serde_json::json!({"state": "x", "instructions": "Which?", "criteria": wide}),
            ))
            .await;
        assert!(!out.is_error, "255 options are accepted: {}", text_of(&out));

        let score = AskScore { facade: mock.clone() };
        let out = score.call(input(serde_json::json!({"state": "x", "instructions": "How bad?", "criteria": ["low", "high"]}))).await;
        assert!(!out.is_error, "two levels are accepted: {}", text_of(&out));

        assert_eq!(mock.calls.lock().len(), 3, "every boundary case reached the facade");
    }

    #[tokio::test]
    async fn ask_score_rejects_one_level() {
        let mock = Arc::new(MockSystemOneFacade::new());
        let tool = AskScore { facade: mock.clone() };

        let out = tool
            .call(input(
                serde_json::json!({"state": "x", "instructions": "How bad?", "criteria": ["only"]}),
            ))
            .await;

        assert!(out.is_error);
        assert!(text_of(&out).contains("at least 2 levels (got 1)"), "{}", text_of(&out));
        assert!(mock.calls.lock().is_empty());
    }

    #[tokio::test]
    async fn ask_score_rejects_eleven_levels() {
        let mock = Arc::new(MockSystemOneFacade::new());
        let tool = AskScore { facade: mock.clone() };
        let criteria: Vec<String> = (0..11).map(|index| format!("level {index}")).collect();

        let out = tool
            .call(input(
                serde_json::json!({"state": "x", "instructions": "How bad?", "criteria": criteria}),
            ))
            .await;

        assert!(out.is_error);
        assert!(text_of(&out).contains("11 levels (max 10)"), "{}", text_of(&out));
        assert!(mock.calls.lock().is_empty());
    }

    #[tokio::test]
    async fn a_401_names_the_config_key() {
        let mock = Arc::new(MockSystemOneFacade::new());
        *mock.result.lock() =
            Some(Err(SystemOneError::Http { status: 401, body: "{\"detail\":[]}".to_owned() }));
        let tool = AskNoul { facade: mock.clone() };

        let out = tool.call(input(serde_json::json!({"state": "x", "instructions": "y"}))).await;

        assert!(out.is_error);
        assert!(text_of(&out).contains("rejected the API key"), "{}", text_of(&out));
        assert!(text_of(&out).contains("`api_key` in forge.toml [systemone]"), "{}", text_of(&out));
    }

    #[tokio::test]
    async fn provider_errors_carry_status_and_body() {
        let mock = Arc::new(MockSystemOneFacade::new());
        *mock.result.lock() = Some(Err(SystemOneError::Http {
            status: 422,
            body: "{\"detail\":[{\"msg\":\"Field required\"}]}".to_owned(),
        }));
        let tool = AskNoul { facade: mock.clone() };

        let out = tool.call(input(serde_json::json!({"state": "x", "instructions": "y"}))).await;

        assert!(out.is_error);
        assert!(text_of(&out).contains("HTTP 422"), "{}", text_of(&out));
        assert!(text_of(&out).contains("Field required"), "{}", text_of(&out));
    }

    #[tokio::test]
    async fn a_transport_failure_names_itself() {
        let mock = Arc::new(MockSystemOneFacade::new());
        *mock.result.lock() = Some(Err(SystemOneError::Transport("connection refused".to_owned())));
        let tool = AskNoul { facade: mock.clone() };

        let out = tool.call(input(serde_json::json!({"state": "x", "instructions": "y"}))).await;

        assert!(out.is_error);
        assert!(
            text_of(&out).contains("System One request failed: connection refused"),
            "{}",
            text_of(&out)
        );
    }

    #[tokio::test]
    async fn timeout_and_invalid_response_map_to_their_texts() {
        let mock = Arc::new(MockSystemOneFacade::new());
        let tool = AskNoul { facade: mock.clone() };

        *mock.result.lock() = Some(Err(SystemOneError::Timeout));
        let out = tool.call(input(serde_json::json!({"state": "x", "instructions": "y"}))).await;
        assert!(out.is_error);
        assert!(text_of(&out).contains("timed out"), "{}", text_of(&out));
        assert!(
            text_of(&out).contains("timeout_ms"),
            "the hint half is the deviation the PR body rests on: {}",
            text_of(&out)
        );

        *mock.result.lock() =
            Some(Err(SystemOneError::InvalidResponse("probability keys drift".to_owned())));
        let out = tool.call(input(serde_json::json!({"state": "x", "instructions": "y"}))).await;
        assert!(out.is_error);
        assert!(
            text_of(&out).contains("invalid answer: probability keys drift"),
            "{}",
            text_of(&out)
        );
    }
}
