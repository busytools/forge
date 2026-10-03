//! `SystemOneFacade` - the seam between the systemone MCP tools and the
//! boot-built decision client. Production holds the client directly
//! (asking needs nothing else of the workspace); the mock records calls
//! and answers canned outcomes for tool tests.

use std::sync::Arc;

use forge_system_one::{AskOutcome, Question, SystemOneClient, SystemOneError};

/// The systemone tools' view: one question, one typed answer.
#[async_trait::async_trait]
pub(crate) trait SystemOneFacade: Send + Sync {
    async fn ask(
        &self,
        state: serde_json::Value,
        question: Question,
    ) -> Result<AskOutcome, SystemOneError>;
}

/// Production facade: straight to the workspace's configured client.
pub(crate) struct ProdSystemOneFacade {
    client: Arc<SystemOneClient>,
}

impl ProdSystemOneFacade {
    pub(crate) fn new(client: Arc<SystemOneClient>) -> Self {
        Self { client }
    }

    pub(crate) fn into_arc(self) -> Arc<dyn SystemOneFacade> {
        Arc::new(self)
    }
}

#[async_trait::async_trait]
impl SystemOneFacade for ProdSystemOneFacade {
    async fn ask(
        &self,
        state: serde_json::Value,
        question: Question,
    ) -> Result<AskOutcome, SystemOneError> {
        self.client.ask(&state, &question).await
    }
}

#[cfg(test)]
#[derive(Default)]
pub(crate) struct MockSystemOneFacade {
    pub result: parking_lot::Mutex<Option<Result<AskOutcome, SystemOneError>>>,
    pub calls: parking_lot::Mutex<Vec<(serde_json::Value, Question)>>,
}

#[cfg(test)]
impl MockSystemOneFacade {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn into_arc(self) -> Arc<dyn SystemOneFacade> {
        Arc::new(self)
    }
}

#[cfg(test)]
#[async_trait::async_trait]
impl SystemOneFacade for MockSystemOneFacade {
    async fn ask(
        &self,
        state: serde_json::Value,
        question: Question,
    ) -> Result<AskOutcome, SystemOneError> {
        self.calls.lock().push((state, question));
        self.result.lock().clone().unwrap_or_else(default_outcome)
    }
}

#[cfg(test)]
fn default_outcome() -> Result<AskOutcome, SystemOneError> {
    Ok(AskOutcome {
        model: "test-model".to_owned(),
        answer: forge_system_one::Answer::Noul { noul: 0.5 },
        usage: Some(forge_system_one::Usage { input_tokens: 1, output_tokens: 1, cost: None }),
    })
}
