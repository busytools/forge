//! The System One wire shapes: one typed question out, one typed answer
//! back, and the check an answer must survive before it is trusted.

use std::collections::{BTreeMap, BTreeSet};

/// One typed question; serialized as the request's `questions` value.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Question {
    Noul {
        instructions: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        criteria: Option<NoulCriteria>,
    },
    Choice {
        instructions: String,
        criteria: BTreeMap<String, Option<String>>,
    },
    Score {
        instructions: String,
        criteria: Vec<String>,
    },
}

/// What yes and no mean for a noul question, when the boundary matters.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct NoulCriteria {
    #[serde(rename = "true")]
    pub r#true: String,
    #[serde(rename = "false")]
    pub r#false: String,
}

/// One typed answer, deserialized from the response.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Answer {
    Noul {
        noul: f64,
    },
    Choice {
        choice: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        probabilities: Option<BTreeMap<String, f64>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        confidence: Option<f64>,
    },
    Score {
        score: f64,
        #[serde(skip_serializing_if = "Option::is_none")]
        probabilities: Option<BTreeMap<String, f64>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        confidence: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        legend: Option<BTreeMap<String, String>>,
    },
}

/// Token usage as the provider reports it; `cost` only on OpenRouter.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
}

/// A resolved decision: what answered, the answer, and what it cost, if
/// the provider reported a usage block.
#[derive(Debug, Clone, PartialEq)]
pub struct AskOutcome {
    pub model: String,
    pub answer: Answer,
    pub usage: Option<Usage>,
}

/// The answer must be internally consistent with the question it answers:
/// keys cover the criteria, probabilities are probabilities, the score is
/// inside its levels. Anything else is a provider bug, not a decision.
pub fn validate_answer(question: &Question, answer: &Answer) -> Result<(), String> {
    match (question, answer) {
        (Question::Noul { .. }, Answer::Noul { noul }) => check_unit(*noul, "noul"),
        (
            Question::Choice { criteria, .. },
            Answer::Choice { choice, probabilities, confidence },
        ) => {
            if !criteria.contains_key(choice) {
                return Err(format!("choice `{choice}` is not one of the criteria keys"));
            }
            if let Some(probabilities) = probabilities {
                let expected: BTreeSet<String> = criteria.keys().cloned().collect();
                check_probability_map(probabilities, &expected)?;
            }
            check_confidence(*confidence)
        }
        (
            Question::Score { criteria, .. },
            Answer::Score { score, probabilities, confidence, legend },
        ) => {
            let levels = criteria.len();
            let Some(last) = levels.checked_sub(1) else {
                return Err("score question defines no levels".to_owned());
            };
            let Ok(last) = u32::try_from(last) else {
                return Err("score question has more levels than supported".to_owned());
            };
            let score = *score;
            if !score.is_finite() || score < 0.0 || score > f64::from(last) {
                return Err(format!("score {score} outside 0..={last}"));
            }
            let expected: BTreeSet<String> = (0..=last).map(|level| level.to_string()).collect();
            if let Some(probabilities) = probabilities {
                check_probability_map(probabilities, &expected)?;
            }
            if let Some(legend) = legend {
                let keys: BTreeSet<&String> = legend.keys().collect();
                let want: BTreeSet<&String> = expected.iter().collect();
                if keys != want {
                    return Err(format!("legend keys {keys:?} do not match the levels {want:?}"));
                }
            }
            check_confidence(*confidence)
        }
        _ => Err("answer type does not match the question type".to_owned()),
    }
}

/// A 0..=1 field: noul, a probability, a confidence.
fn check_unit(value: f64, field: &str) -> Result<(), String> {
    if !value.is_finite() || !(0.0..=1.0).contains(&value) {
        return Err(format!("{field} {value} outside 0..=1"));
    }
    Ok(())
}

fn check_confidence(confidence: Option<f64>) -> Result<(), String> {
    match confidence {
        Some(confidence) => check_unit(confidence, "confidence"),
        None => Ok(()),
    }
}

/// Keys, values, and the sum tolerance pydantic-ai applies for the
/// provider's two-decimal display: 1e-6 + n * 0.005.
fn check_probability_map(
    probabilities: &BTreeMap<String, f64>,
    expected: &BTreeSet<String>,
) -> Result<(), String> {
    let keys: BTreeSet<&String> = probabilities.keys().collect();
    let want: BTreeSet<&String> = expected.iter().collect();
    if keys != want {
        return Err(format!("probability keys {keys:?} do not match the expected keys {want:?}"));
    }
    let mut sum = 0.0;
    for value in probabilities.values() {
        check_unit(*value, "probability")?;
        sum += value;
    }
    let count = u32::try_from(probabilities.len()).unwrap_or(u32::MAX);
    let tolerance = 1e-6 + f64::from(count) * 0.005;
    if (sum - 1.0).abs() > tolerance {
        return Err(format!("probabilities sum to {sum} (expected ~1)"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noul_question() -> Question {
        Question::Noul { instructions: "Is this a bug?".to_owned(), criteria: None }
    }

    fn choice_question() -> Question {
        Question::Choice {
            instructions: "Which team?".to_owned(),
            criteria: [
                ("billing".to_owned(), Some("Payments and refunds".to_owned())),
                ("technical".to_owned(), None),
            ]
            .into_iter()
            .collect(),
        }
    }

    fn score_question() -> Question {
        Question::Score {
            instructions: "How urgent?".to_owned(),
            criteria: vec!["Routine".to_owned(), "Soon".to_owned(), "Urgent".to_owned()],
        }
    }

    fn probs(pairs: &[(&str, f64)]) -> std::collections::BTreeMap<String, f64> {
        pairs.iter().map(|(key, value)| ((*key).to_owned(), *value)).collect()
    }

    #[test]
    fn question_serializes_to_the_wire_shape() {
        let q = Question::Noul {
            instructions: "Is this a billing issue?".to_owned(),
            criteria: Some(NoulCriteria {
                r#true: "Payments or refunds".to_owned(),
                r#false: "Anything else".to_owned(),
            }),
        };
        assert_eq!(
            serde_json::to_value(&q).unwrap(),
            serde_json::json!({"type":"noul","instructions":"Is this a billing issue?","criteria":{"true":"Payments or refunds","false":"Anything else"}})
        );
        let c = Question::Choice {
            instructions: "Which team?".to_owned(),
            criteria: [
                ("billing".to_owned(), Some("Payments".to_owned())),
                ("frontend".to_owned(), None),
            ]
            .into_iter()
            .collect(),
        };
        assert_eq!(
            serde_json::to_value(&c).unwrap(),
            serde_json::json!({"type":"choice","instructions":"Which team?","criteria":{"billing":"Payments","frontend":null}})
        );
        let s = Question::Score {
            instructions: "How urgent?".to_owned(),
            criteria: vec!["Routine".to_owned(), "Soon".to_owned(), "Urgent".to_owned()],
        };
        assert_eq!(
            serde_json::to_value(&s).unwrap(),
            serde_json::json!({"type":"score","instructions":"How urgent?","criteria":["Routine","Soon","Urgent"]})
        );
    }

    #[test]
    fn noul_without_criteria_omits_the_field() {
        assert_eq!(
            serde_json::to_value(noul_question()).unwrap(),
            serde_json::json!({"type":"noul","instructions":"Is this a bug?"})
        );
    }

    #[test]
    fn noul_answer_ignores_extra_payload_fields() {
        let answer: Answer =
            serde_json::from_value(serde_json::json!({"type":"noul","noul":0.5,"extra":1}))
                .unwrap();
        assert_eq!(answer, Answer::Noul { noul: 0.5 });
    }

    #[test]
    fn choice_answer_probabilities_default_to_none() {
        let answer: Answer =
            serde_json::from_value(serde_json::json!({"type":"choice","choice":"billing"}))
                .unwrap();
        assert_eq!(
            answer,
            Answer::Choice { choice: "billing".to_owned(), probabilities: None, confidence: None }
        );
    }

    /// The tool output serializes an answer back out; absent optionals
    /// must not come back as nulls.
    #[test]
    fn answer_serializes_without_absent_option_fields() {
        let answer =
            Answer::Choice { choice: "billing".to_owned(), probabilities: None, confidence: None };
        assert_eq!(
            serde_json::to_value(&answer).unwrap(),
            serde_json::json!({"type":"choice","choice":"billing"})
        );
        let usage = Usage { input_tokens: 10, output_tokens: 3, cost: None };
        assert_eq!(
            serde_json::to_value(&usage).unwrap(),
            serde_json::json!({"input_tokens":10,"output_tokens":3})
        );
    }

    #[test]
    fn noul_in_range_validates() {
        assert_eq!(validate_answer(&noul_question(), &Answer::Noul { noul: 0.83 }), Ok(()));
    }

    #[test]
    fn noul_out_of_range_is_refused() {
        assert_eq!(
            validate_answer(&noul_question(), &Answer::Noul { noul: 1.5 }),
            Err("noul 1.5 outside 0..=1".to_owned())
        );
    }

    #[test]
    fn noul_nan_is_refused() {
        assert_eq!(
            validate_answer(&noul_question(), &Answer::Noul { noul: f64::NAN }),
            Err("noul NaN outside 0..=1".to_owned())
        );
    }

    #[test]
    fn choice_in_criteria_validates() {
        let answer = Answer::Choice {
            choice: "billing".to_owned(),
            probabilities: Some(probs(&[("billing", 0.9), ("technical", 0.1)])),
            confidence: Some(0.8),
        };
        assert_eq!(validate_answer(&choice_question(), &answer), Ok(()));
    }

    #[test]
    fn choice_outside_criteria_is_refused() {
        let answer =
            Answer::Choice { choice: "nope".to_owned(), probabilities: None, confidence: None };
        assert_eq!(
            validate_answer(&choice_question(), &answer),
            Err("choice `nope` is not one of the criteria keys".to_owned())
        );
    }

    #[test]
    fn choice_probability_key_drift_is_refused() {
        let answer = Answer::Choice {
            choice: "billing".to_owned(),
            probabilities: Some(probs(&[("billing", 0.9), ("sales", 0.1)])),
            confidence: None,
        };
        let err =
            validate_answer(&choice_question(), &answer).expect_err("drifted keys are refused");
        assert!(err.starts_with("probability keys"), "{err}");
    }

    #[test]
    fn probabilities_wrong_sum_is_refused() {
        let answer = Answer::Choice {
            choice: "billing".to_owned(),
            probabilities: Some(probs(&[("billing", 0.5), ("technical", 0.2)])),
            confidence: None,
        };
        assert_eq!(
            validate_answer(&choice_question(), &answer),
            Err("probabilities sum to 0.7 (expected ~1)".to_owned())
        );
    }

    #[test]
    fn probabilities_within_display_tolerance_validate() {
        // 0.005 of drift on two options sits inside 1e-6 + 2 * 0.005.
        let answer = Answer::Choice {
            choice: "billing".to_owned(),
            probabilities: Some(probs(&[("billing", 0.995), ("technical", 0.01)])),
            confidence: None,
        };
        assert_eq!(validate_answer(&choice_question(), &answer), Ok(()));
    }

    #[test]
    fn probability_value_out_of_range_is_refused() {
        let answer = Answer::Choice {
            choice: "billing".to_owned(),
            probabilities: Some(probs(&[("billing", 1.5), ("technical", -0.5)])),
            confidence: None,
        };
        let err = validate_answer(&choice_question(), &answer)
            .expect_err("out-of-range probability is refused");
        assert!(err.starts_with("probability 1.5 outside 0..=1"), "{err}");
    }

    #[test]
    fn confidence_out_of_range_is_refused() {
        let answer = Answer::Choice {
            choice: "billing".to_owned(),
            probabilities: None,
            confidence: Some(1.2),
        };
        assert_eq!(
            validate_answer(&choice_question(), &answer),
            Err("confidence 1.2 outside 0..=1".to_owned())
        );
    }

    #[test]
    fn score_in_range_validates() {
        let answer = Answer::Score {
            score: 1.79,
            probabilities: Some(probs(&[("0", 0.1), ("1", 0.3), ("2", 0.6)])),
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
        assert_eq!(validate_answer(&score_question(), &answer), Ok(()));
    }

    #[test]
    fn score_outside_levels_is_refused() {
        let answer =
            Answer::Score { score: 2.5, probabilities: None, confidence: None, legend: None };
        assert_eq!(
            validate_answer(&score_question(), &answer),
            Err("score 2.5 outside 0..=2".to_owned())
        );
    }

    #[test]
    fn score_probability_key_drift_is_refused() {
        let answer = Answer::Score {
            score: 1.0,
            probabilities: Some(probs(&[("0", 0.5), ("2", 0.5)])),
            confidence: None,
            legend: None,
        };
        let err = validate_answer(&score_question(), &answer)
            .expect_err("a missing level key is refused");
        assert!(err.starts_with("probability keys"), "{err}");
    }

    #[test]
    fn legend_key_drift_is_refused() {
        let answer = Answer::Score {
            score: 1.0,
            probabilities: None,
            confidence: None,
            legend: Some(
                [("0".to_owned(), "Routine".to_owned()), ("5".to_owned(), "Nope".to_owned())]
                    .into_iter()
                    .collect(),
            ),
        };
        let err =
            validate_answer(&score_question(), &answer).expect_err("a drifted legend is refused");
        assert!(err.starts_with("legend keys"), "{err}");
    }

    #[test]
    fn answer_type_mismatch_is_refused() {
        let answer =
            Answer::Choice { choice: "billing".to_owned(), probabilities: None, confidence: None };
        assert_eq!(
            validate_answer(&noul_question(), &answer),
            Err("answer type does not match the question type".to_owned())
        );
    }

    #[test]
    fn score_question_with_no_levels_is_refused() {
        let question =
            Question::Score { instructions: "How urgent?".to_owned(), criteria: Vec::new() };
        let answer =
            Answer::Score { score: 0.0, probabilities: None, confidence: None, legend: None };
        assert_eq!(
            validate_answer(&question, &answer),
            Err("score question defines no levels".to_owned())
        );
    }
}
