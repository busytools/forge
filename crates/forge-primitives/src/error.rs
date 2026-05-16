//! App-level error enum used across forge crates.
//! `forge_workspace::SessionUpdate::FatalError` carries this type.

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AppError {
    #[error("Agent bridge connection failed")]
    ConnectionFailed,
    #[error("Authentication required")]
    AuthRequired,
}

impl AppError {
    pub const CONNECTION_FAILED_EXIT_CODE: i32 = 22;
    pub const AUTH_REQUIRED_EXIT_CODE: i32 = 24;

    pub fn exit_code(&self) -> i32 {
        match self {
            Self::ConnectionFailed => Self::CONNECTION_FAILED_EXIT_CODE,
            Self::AuthRequired => Self::AUTH_REQUIRED_EXIT_CODE,
        }
    }

    pub fn user_message(&self) -> &'static str {
        match self {
            Self::ConnectionFailed => {
                "Failed to establish or maintain the Agent SDK bridge connection."
            }
            Self::AuthRequired => {
                "Authentication required. Run `claude auth login` in a terminal to authenticate."
            }
        }
    }
}
