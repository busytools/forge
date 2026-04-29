use crate::agent::wire::CommandEnvelope;
use tokio::sync::mpsc;

pub struct AgentConnection {
    command_tx: mpsc::UnboundedSender<CommandEnvelope>,
}
