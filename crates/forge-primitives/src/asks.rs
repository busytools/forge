//! The ask seam: what a subsystem holds for a person, and how it reports
//! that to the host that numbers and routes it.
//!
//! **One predicate decides what a seat is waiting on, and every ask source
//! is on its list.** A question and a permission prompt park in the
//! session's own pending set; a Slack draft, the browser, terminal and
//! document hand-offs, a settings write and a vault call park in a
//! subsystem's own registry. The split between those places is what let a
//! terminal hand-off draw in the dock and mark nothing (#2954), the third
//! time a kind was missed. So a subsystem reports what it holds through
//! [`AskSource`], the host numbers each arrival through [`AskSink`], and
//! the host's census is built from the sources it registered rather than
//! from a list someone has to remember to edit.
//!
//! [`PendingAsk`] is the record a view draws; [`PendingInteractionKind`] is
//! the kind a needs-you row names. Both live here because a port has to
//! name them without depending on the host.

use uuid::Uuid;

use crate::SessionSlot;
use crate::permission_interaction::PermissionRequest;
use crate::question::QuestionRequest;

/// What a session is waiting on a person for. The kind a needs-you row
/// names, since "asked you a question" and "a permission prompt is
/// waiting" are different asks with the same mark.
///
/// **One variant per ask source, and every source is here.** A new ask kind
/// fails the build in the host's guard test until the source that raises
/// it names it, and the guard parks one of each and asserts the needs-you
/// path answers it - so a kind cannot be named without being covered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PendingInteractionKind {
    Question,
    Permission,
    SlackDraft,
    BrowserHandOff,
    TerminalHandOff,
    DocumentHandOff,
    SettingsWrite,
    Vault,
}

/// What a seat is holding, as the core kept it: the prompt a view that
/// attached late has no other way to read.
///
/// The kinds are every member of the category, not the two the dock
/// happened to draw first: a Slack draft parks on a reply the same way a
/// permission and a question do - and a browser hand-off parks on a person -
/// so a record carrying two of them told a builder the category was covered
/// when it was not.
#[derive(Clone)]
pub enum PendingAsk {
    Permission(Box<PermissionRequest>),
    Question(Box<QuestionRequest>),
    SlackDraft(Box<crate::slack::SlackDraft>),
    BrowserHandOff(Box<crate::browser::HandOff>),
    TerminalHandOff(Box<crate::terminal::TerminalHandOff>),
    DocumentHandOff(Box<crate::document::DocumentHandOffAsk>),
    SettingsWrite(Box<crate::settings::SettingsWriteAsk>),
    Vault(Box<crate::vault::VaultDraft>),
}

impl PendingAsk {
    /// The tool call this prompt is about, which is the id an answer names.
    ///
    /// `None` for a Slack draft and a hand-off: each is answered by its own
    /// command with its own id, and names no tool call at all.
    pub fn tool_id(&self) -> Option<&str> {
        match self {
            Self::Permission(request) => Some(&request.tool_call.tool_call_id),
            Self::Question(request) => Some(&request.tool_call.tool_call_id),
            Self::SlackDraft(_)
            | Self::BrowserHandOff(_)
            | Self::TerminalHandOff(_)
            | Self::DocumentHandOff(_)
            | Self::SettingsWrite(_)
            | Self::Vault(_) => None,
        }
    }
}

/// One ask a source is holding: its identity, the kind a needs-you row
/// names, and the record a dock draws.
#[derive(Clone)]
pub struct Ask {
    pub id: Uuid,
    pub kind: PendingInteractionKind,
    pub payload: PendingAsk,
}

/// A subsystem that holds asks for a seat.
///
/// The host reads this for `pending_asks`, `pending_interaction` and
/// `pending_interaction_depth`, so a source's own registry is the truth
/// about what it holds - there is no second copy in the host to drift.
pub trait AskSource: Send + Sync {
    /// Every ask this source holds for `seat`, in the order it should be
    /// drawn.
    fn asks(&self, seat: &SessionSlot) -> Vec<Ask>;

    /// The kinds this source can raise. The host's guard walks the
    /// registered sources' lists, so a source that raises a kind it does
    /// not name here is not covered by the census.
    fn kinds(&self) -> &'static [PendingInteractionKind];
}

/// The host a subsystem reports its asks to: the number an arrival carries
/// and the settlement that follows an answer.
///
/// **The number is the ask's identity on the wire.** A client announces a
/// new ask by it, so a re-read of the roster is not news, an ask replacing
/// another at the same depth is, and an answer that shrinks a batch is
/// not. A source reports the arrival; the host decides what the number
/// means.
pub trait AskSink: Send + Sync {
    /// An ask arrived for `seat`, of `kind`.
    fn note(&self, seat: &SessionSlot, kind: PendingInteractionKind);

    /// The ask `id` for `seat` settled and is no longer held. The source's
    /// own read is what clears the host's mark; this tells the host the
    /// settlement happened rather than leaving it to be inferred.
    fn resolve(&self, seat: &SessionSlot, id: Uuid);
}
