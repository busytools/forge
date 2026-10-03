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
use forge_sdk::mcp::tool::{Tool, ToolInput, ToolOutput, ToolOutputBlock};
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
    ToolOutput { blocks: vec![ToolOutputBlock { text }], is_error: true }
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
            let true_text = criteria.remove("true");
            let false_text = criteria.remove("false");
            match (true_text, false_text, criteria.is_empty()) {
                (Some(true_text), Some(false_text), true) => {
                    Some(NoulCriteria { r#true: true_text, r#false: false_text })
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
         When to reach for it: before interrupting the user with a question this session could \
         probably decide itself (put the situation in `state` and the ask in `instructions`; \
         when the answer is decisive and the action reversible, act), or as a second opinion \
         when you are leaning one way and want it checked. One question per call; several \
         questions about one state are several parallel calls. Reading the answer: `noul` is \
         the yes-probability; near 0 or 1 is decisive, near 0.5 is genuine uncertainty. The \
         threshold belongs to you, set per question from what being wrong would cost; when the \
         answer is not certain enough, ask the user instead of guessing. Boundaries: these are \
         bounded estimates, not guarantees; the model class is weak at arithmetic, counting, \
         and date comparison, so compute numbers and dates in code and ask it for judgments. \
         The model sees only `state` and this question, nothing else reaches it, so fold every \
         needed definition into `instructions` and `criteria` (what should make it yes, what \
         no), and when several labels may each apply, ask one noul per label."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "state": { "description": "The material to judge: a string, a JSON object, or an array. Give it the full context the judgment needs - what the user asked for, the current state, and any policy or facts that bear on the answer; prefer named JSON fields when the context has more than one part." },
                "instructions": { "type": "string", "description": "The yes/no question itself." },
                "criteria": {
                    "type": "object",
                    "properties": {
                        "true": { "type": "string", "description": "What makes the answer yes." },
                        "false": { "type": "string", "description": "What makes the answer no." },
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
         writes prose. When to reach for it: routing and picking between enumerated \
         alternatives, or as a second opinion when you are leaning toward one option and want \
         the alternatives weighed. Enumerate every option in `criteria` and describe when each \
         applies (at most 255 options); when nothing may fit the state, include an explicit \
         no-match option, because the model can only choose among the options you list. Reading \
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
                "state": { "description": "The material to judge: a string, a JSON object, or an array. Give it the full context the judgment needs - what the user asked for, the current state, and any policy or facts that bear on the answer; prefer named JSON fields when the context has more than one part." },
                "instructions": { "type": "string", "description": "The question the options answer." },
                "criteria": {
                    "type": "object",
                    "additionalProperties": { "type": ["string", "null"] },
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
         your text, and a confidence. The model never writes prose. When to reach for it: \
         severity, quality, priority, or risk judgments where the levels are meaningful to you. \
         Define `criteria` as an ordered list of level descriptions, lowest first (two to ten \
         levels); each level should describe a concrete situation and stand on its own. Reading \
         the answer: `score` is the probability-weighted position (for example 1.79 on a \
         three-level rubric); `probabilities` shows how settled the answer is; `confidence` is \
         a statistic of the distribution, not a calibrated chance of being right; the threshold \
         belongs to you, set from what being wrong would cost. Boundaries: each level \
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
                "state": { "description": "The material to judge: a string, a JSON object, or an array. Give it the full context the judgment needs - what the user asked for, the current state, and any policy or facts that bear on the answer; prefer named JSON fields when the context has more than one part." },
                "instructions": { "type": "string", "description": "The rubric question itself." },
                "criteria": {
                    "type": "array",
                    "items": { "type": "string" },
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
            out.blocks[0].text
        );
        assert!(
            !out.blocks[0].text.contains("usage"),
            "no invented usage block: {}",
            out.blocks[0].text
        );
        assert!(out.blocks[0].text.contains("\"noul\":0.5"), "{}", out.blocks[0].text);
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

        assert!(!out.is_error, "noul succeeds: {}", out.blocks[0].text);
        assert!(out.blocks[0].text.contains("\"noul\":0.83"), "{}", out.blocks[0].text);
        assert!(out.blocks[0].text.contains("test-model"), "{}", out.blocks[0].text);
        let calls = mock.calls.lock();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, serde_json::json!({"ticket": "x"}));
        assert_eq!(
            calls[0].1,
            Question::Noul {
                instructions: "Is this a billing issue?".to_owned(),
                criteria: Some(NoulCriteria {
                    r#true: "Payments".to_owned(),
                    r#false: "Anything else".to_owned()
                }),
            }
        );
    }

    #[tokio::test]
    async fn ask_noul_criteria_needs_exactly_true_and_false() {
        let mock = Arc::new(MockSystemOneFacade::new());
        let tool = AskNoul { facade: mock.clone() };

        let out = tool.call(input(serde_json::json!({"state": "x", "instructions": "Is it?", "criteria": {"true": "yes"}}))).await;

        assert!(out.is_error);
        assert!(
            out.blocks[0].text.contains("exactly the keys `true` and `false`"),
            "{}",
            out.blocks[0].text
        );
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

        assert!(!out.is_error, "choice succeeds: {}", out.blocks[0].text);
        assert!(out.blocks[0].text.contains("\"choice\":\"billing\""), "{}", out.blocks[0].text);
        assert!(out.blocks[0].text.contains("\"probabilities\""), "{}", out.blocks[0].text);
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
        assert!(out.blocks[0].text.contains("at least one option"), "{}", out.blocks[0].text);
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
        assert!(out.blocks[0].text.contains("256 options (max 255)"), "{}", out.blocks[0].text);
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

        assert!(!out.is_error, "score succeeds: {}", out.blocks[0].text);
        assert!(out.blocks[0].text.contains("\"score\":1.79"), "{}", out.blocks[0].text);
        assert!(out.blocks[0].text.contains("\"legend\""), "{}", out.blocks[0].text);
        assert!(out.blocks[0].text.contains("\"probabilities\""), "{}", out.blocks[0].text);
        let calls = mock.calls.lock();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].1,
            Question::Score {
                instructions: "How urgent?".to_owned(),
                criteria: vec!["Routine".to_owned(), "Soon".to_owned(), "Urgent".to_owned()],
            }
        );
    }

    #[tokio::test]
    async fn boundary_criteria_are_accepted_and_reach_the_facade() {
        let mock = Arc::new(MockSystemOneFacade::new());
        let choice = AskChoice { facade: mock.clone() };

        let out = choice.call(input(serde_json::json!({"state": "x", "instructions": "Which?", "criteria": {"only": null}}))).await;
        assert!(!out.is_error, "one option is accepted: {}", out.blocks[0].text);

        let wide: serde_json::Map<String, serde_json::Value> =
            (0..255).map(|index| (format!("o{index}"), serde_json::Value::Null)).collect();
        let out = choice
            .call(input(
                serde_json::json!({"state": "x", "instructions": "Which?", "criteria": wide}),
            ))
            .await;
        assert!(!out.is_error, "255 options are accepted: {}", out.blocks[0].text);

        let score = AskScore { facade: mock.clone() };
        let out = score.call(input(serde_json::json!({"state": "x", "instructions": "How bad?", "criteria": ["low", "high"]}))).await;
        assert!(!out.is_error, "two levels are accepted: {}", out.blocks[0].text);

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
        assert!(out.blocks[0].text.contains("at least 2 levels (got 1)"), "{}", out.blocks[0].text);
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
        assert!(out.blocks[0].text.contains("11 levels (max 10)"), "{}", out.blocks[0].text);
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
        assert!(out.blocks[0].text.contains("rejected the API key"), "{}", out.blocks[0].text);
        assert!(
            out.blocks[0].text.contains("`api_key` in forge.toml [systemone]"),
            "{}",
            out.blocks[0].text
        );
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
        assert!(out.blocks[0].text.contains("HTTP 422"), "{}", out.blocks[0].text);
        assert!(out.blocks[0].text.contains("Field required"), "{}", out.blocks[0].text);
    }

    #[tokio::test]
    async fn a_transport_failure_names_itself() {
        let mock = Arc::new(MockSystemOneFacade::new());
        *mock.result.lock() = Some(Err(SystemOneError::Transport("connection refused".to_owned())));
        let tool = AskNoul { facade: mock.clone() };

        let out = tool.call(input(serde_json::json!({"state": "x", "instructions": "y"}))).await;

        assert!(out.is_error);
        assert!(
            out.blocks[0].text.contains("System One request failed: connection refused"),
            "{}",
            out.blocks[0].text
        );
    }

    #[tokio::test]
    async fn timeout_and_invalid_response_map_to_their_texts() {
        let mock = Arc::new(MockSystemOneFacade::new());
        let tool = AskNoul { facade: mock.clone() };

        *mock.result.lock() = Some(Err(SystemOneError::Timeout));
        let out = tool.call(input(serde_json::json!({"state": "x", "instructions": "y"}))).await;
        assert!(out.is_error);
        assert!(out.blocks[0].text.contains("timed out"), "{}", out.blocks[0].text);

        *mock.result.lock() =
            Some(Err(SystemOneError::InvalidResponse("probability keys drift".to_owned())));
        let out = tool.call(input(serde_json::json!({"state": "x", "instructions": "y"}))).await;
        assert!(out.is_error);
        assert!(
            out.blocks[0].text.contains("invalid answer: probability keys drift"),
            "{}",
            out.blocks[0].text
        );
    }
}
