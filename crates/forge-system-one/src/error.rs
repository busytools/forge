/// Why a System One call failed; the tool layer words its message from
/// these variants.
#[derive(Debug, thiserror::Error)]
pub enum SystemOneError {
    #[error("System One request failed: {0}")]
    Transport(String),
    #[error("System One request timed out")]
    Timeout,
    #[error("System One returned HTTP {status}: {body}")]
    Http { status: u16, body: String },
    #[error("System One returned an invalid answer: {0}")]
    InvalidResponse(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_carries_the_failure_shape() {
        assert_eq!(SystemOneError::Timeout.to_string(), "System One request timed out");
        assert_eq!(SystemOneError::Transport("boom".to_owned()).to_string(), "System One request failed: boom");
        assert_eq!(SystemOneError::Http { status: 422, body: "detail".to_owned() }.to_string(), "System One returned HTTP 422: detail");
        assert_eq!(SystemOneError::InvalidResponse("keys".to_owned()).to_string(), "System One returned an invalid answer: keys");
    }
}
