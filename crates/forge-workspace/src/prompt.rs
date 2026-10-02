//! What a composer's text means at forge's own boundary.
//!
//! One rule, in one place: text that names a forge command is forge's, not
//! the `claude` CLI's. The terminal has always decided this in its own submit
//! path; a client reaches the same commands only because the decision is made
//! where both of them dispatch, since some of these names the CLI does not
//! have at all and `/new` it answers as a different command.

/// A forge command a composer can invoke, whatever view it was typed at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForgePrompt {
    /// `/new`: replace the seat's occupant with a fresh session.
    NewSession,
}

impl ForgePrompt {
    /// Every command [`forge_prompt`] answers.
    pub const ALL: &'static [Self] = &[Self::NewSession];

    /// The name as it is typed.
    pub const fn name(self) -> &'static str {
        match self {
            Self::NewSession => "/new",
        }
    }
}

/// The forge command `text` names, or `None` when the text is the reader's
/// own prose.
///
/// The command has to be the whole text: `/newer` is a message to the model
/// rather than a near-miss of `/new`.
pub fn forge_prompt(text: &str) -> Option<ForgePrompt> {
    Some(match text.trim() {
        "/new" => ForgePrompt::NewSession,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lone_command_name_is_forges() {
        assert_eq!(forge_prompt("/new"), Some(ForgePrompt::NewSession));
        assert_eq!(forge_prompt("  /new  "), Some(ForgePrompt::NewSession));
    }

    /// Prose that starts like a command, and a command with words after it,
    /// are both the reader's own text.
    #[test]
    fn anything_but_the_bare_name_is_prose() {
        for text in ["/newer", "/new session", "hello /new", "/NEW", "/new\nsecond line"] {
            assert_eq!(forge_prompt(text), None, "{text:?} is not a forge command");
        }
    }
}
