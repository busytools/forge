use crate::agent::wire::CommandEnvelope;
use tokio::sync::mpsc;

#[allow(dead_code)]
pub struct AgentConnection {
    command_tx: mpsc::UnboundedSender<CommandEnvelope>,
}
