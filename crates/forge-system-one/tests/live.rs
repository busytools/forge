//! Opt-in live smoke: one real decision against the configured endpoint.
//!
//! `SYSTEMONE_BASE_URL=https://api.typesafe.ai SYSTEMONE_API_KEY=... \
//!  cargo nextest run -p forge-system-one --run-ignored all live_typesafe_noul`
//!
//! Ignored by default so CI and `just check` never reach the network.

use forge_system_one::{Answer, NoulCriteria, Question, SystemOneClient, SystemOneConfig};

#[tokio::test]
#[ignore = "live: needs SYSTEMONE_BASE_URL and SYSTEMONE_API_KEY"]
async fn live_typesafe_noul() {
    let Ok(base_url) = std::env::var("SYSTEMONE_BASE_URL") else {
        eprintln!("SYSTEMONE_BASE_URL unset; nothing to smoke");
        return;
    };
    let Ok(api_key) = std::env::var("SYSTEMONE_API_KEY") else {
        eprintln!("SYSTEMONE_API_KEY unset; nothing to smoke");
        return;
    };
    let model = std::env::var("SYSTEMONE_MODEL").unwrap_or_else(|_| "jev-latest".to_owned());
    let config = SystemOneConfig { base_url, api_key: Some(api_key), model, timeout_ms: 30_000 };
    let client = SystemOneClient::new(&config, reqwest::Client::new());

    let question = Question::Noul {
        instructions: "Is this state a greeting?".to_owned(),
        criteria: Some(NoulCriteria { r#true: "The state greets a reader.".to_owned(), r#false: "Anything else.".to_owned() }),
    };
    let outcome = client.ask(&serde_json::json!("Hello, world"), &question).await.expect("live call succeeds");

    assert!(!outcome.model.is_empty(), "the response names the model that answered");
    match outcome.answer {
        Answer::Noul { noul } => {
            assert!((0.0..=1.0).contains(&noul), "noul {noul} is a probability");
            println!("live smoke: model={} noul={noul} usage={:?}", outcome.model, outcome.usage);
        }
        other => panic!("expected a noul answer, got {other:?}"),
    }
}
