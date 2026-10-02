//! What a composer's text means at forge's own boundary.
//!
//! One rule, in one place: text that names a forge command is forge's, not
//! the `claude` CLI's. The terminal has always decided this in its own submit
//! path; a client reaches the same commands only because the decision is made
//! where both of them dispatch, since some of these names the CLI does not
//! have at all and `/new` it answers as a different command.

/// A forge command a composer can invoke, whatever view it was typed at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForgePrompt {
    /// `/new`: replace the seat's occupant with a fresh session.
    NewSession,
    /// `/resume <session_id>`: move the seat onto a session that already
    /// exists.
    ResumeSession { session_id: String },
    /// `/mode <id>`: switch the session's permission mode.
    SetMode { mode: String },
    /// `/model <id>`: switch the model the session runs on.
    SetModel { model: String },
    /// `/effort <level>`: set the thinking effort the next launch carries.
    SetEffort { level: String },
}

/// What one of forge's command names was asked to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invocation {
    /// Invoked the way the command takes it.
    Command(ForgePrompt),
    /// Invoked some other way, with the line the command answers instead of
    /// acting. Answering is what keeps a mistyped command from being read as
    /// prose by the model.
    Misuse(&'static str),
}

impl ForgePrompt {
    /// The name as it is typed.
    pub const fn name(&self) -> &'static str {
        match self {
            Self::NewSession => "/new",
            Self::ResumeSession { .. } => "/resume",
            Self::SetMode { .. } => "/mode",
            Self::SetModel { .. } => "/model",
            Self::SetEffort { .. } => "/effort",
        }
    }
}

/// The names forge answers, and the line each answers a wrong invocation
/// with. The one place a name is written down.
const USAGE: &[(&str, &str)] = &[
    ("/new", "Usage: /new"),
    ("/resume", "Usage: /resume <session_id>"),
    ("/mode", "Usage: /mode <id>"),
    ("/model", "Usage: /model <id>"),
    ("/effort", "Usage: /effort <level>"),
];

/// The names the CLI resolves itself though it does not advertise them, so a
/// view forwards them rather than answering or refusing.
///
/// Measured on 2.1.280 under forge's own argv: `/help`, `/plugins` and
/// `/quit` each come back as a local line ("isn't available in this
/// environment") with no model call, where a name the CLI does not know at
/// all is sent to the model as a question.
pub const FORWARDED: [&str; 4] = ["/help", "/mcp", "/plugins", "/quit"];

/// Every name forge answers, which a view reads to decide to forward rather
/// than refuse.
pub fn forge_prompt_names() -> impl Iterator<Item = &'static str> {
    USAGE.iter().map(|(name, _)| *name)
}

/// Whether `name` is one of forge's command names, invoked well or not.
pub fn is_forge_prompt_name(name: &str) -> bool {
    usage_for(name).is_some()
}

/// Whether the CLI resolves `name` itself, advertised or not.
pub fn is_forwarded_name(name: &str) -> bool {
    FORWARDED.contains(&name)
}

/// The forge command `text` invokes, or `None` when the text is the reader's
/// own prose.
///
/// The name has to be a whole word: `/newer` is a message to the model rather
/// than a near-miss of `/new`.
pub fn forge_invocation(text: &str) -> Option<Invocation> {
    let mut words = text.split_whitespace();
    let name = words.next()?;
    let arguments: Vec<&str> = words.collect();
    let command = match (name, arguments.as_slice()) {
        ("/new", []) => Some(ForgePrompt::NewSession),
        ("/resume", [arg]) => Some(ForgePrompt::ResumeSession { session_id: (*arg).to_owned() }),
        ("/mode", [arg]) => Some(ForgePrompt::SetMode { mode: (*arg).to_owned() }),
        ("/model", [arg]) => Some(ForgePrompt::SetModel { model: (*arg).to_owned() }),
        ("/effort", [arg]) => Some(ForgePrompt::SetEffort { level: (*arg).to_owned() }),
        _ => None,
    };
    match command {
        Some(command) => Some(Invocation::Command(command)),
        None => usage_for(name).map(Invocation::Misuse),
    }
}

fn usage_for(name: &str) -> Option<&'static str> {
    USAGE.iter().find(|(known, _)| *known == name).map(|(_, usage)| *usage)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lone_command_name_is_forges() {
        assert_eq!(forge_invocation("/new"), Some(Invocation::Command(ForgePrompt::NewSession)));
        assert_eq!(
            forge_invocation("  /new  "),
            Some(Invocation::Command(ForgePrompt::NewSession)),
        );
    }

    #[test]
    fn a_command_with_its_argument_carries_it() {
        let named = |session_id: &str| {
            Some(Invocation::Command(ForgePrompt::ResumeSession {
                session_id: session_id.to_owned(),
            }))
        };
        assert_eq!(forge_invocation("/resume 7f3a92e0"), named("7f3a92e0"));
        assert_eq!(
            forge_invocation("  /resume  7f3a92e0  "),
            named("7f3a92e0"),
            "the argument is read the way a shell would read it",
        );
    }

    /// A forge name invoked some other way is still forge's: the reader gets
    /// the command's own usage line rather than a question to the model.
    #[test]
    fn a_wrong_invocation_is_answered_with_its_usage() {
        assert_eq!(forge_invocation("/new session"), Some(Invocation::Misuse("Usage: /new")),);
        assert_eq!(
            forge_invocation("/resume"),
            Some(Invocation::Misuse("Usage: /resume <session_id>")),
        );
        assert_eq!(
            forge_invocation("/resume two ids"),
            Some(Invocation::Misuse("Usage: /resume <session_id>")),
        );
        assert_eq!(
            forge_invocation("/new\nsecond line"),
            Some(Invocation::Misuse("Usage: /new")),
            "a command with words after it is answered, not read as prose",
        );
    }

    /// Prose that starts like a command is the reader's own text.
    #[test]
    fn anything_but_a_whole_command_word_is_prose() {
        for text in ["/newer", "hello /new", "/NEW", "/spinner", "hello", "/ne"] {
            assert_eq!(forge_invocation(text), None, "{text:?} is not a forge command");
        }
    }

    /// The names a view forwards on, which is the wider set: a half-typed
    /// command has to reach the core to be answered.
    #[test]
    fn every_name_is_forwarded_however_it_was_invoked() {
        for name in forge_prompt_names() {
            assert!(is_forge_prompt_name(name), "{name} is forge's");
        }
        assert!(!is_forge_prompt_name("/newer"), "a name that only starts alike is prose");
        assert!(!is_forge_prompt_name("/spinner"), "a terminal-side verb is not a core command");
    }
}
