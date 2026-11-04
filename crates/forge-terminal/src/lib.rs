//! One terminal per seat: the PTY, the screen model, the OSC 133 scanner and
//! the shell integration scripts.
//!
//! **A leaf.** It knows nothing about a seat key, a socket or a session: one
//! value per terminal, spawned with a shell, a cwd and an environment, and a
//! small interface - write, resize, close, read the screen, read the state,
//! and subscribe to the output. The workspace keys it by the slot; the server
//! carries its bytes; the client draws them.
//!
//! `alacritty_terminal` owns the PTY, the parser, `Term` with reflow and
//! scrollback, and the event types. The event loop is forge's own, because
//! the client's stream is the raw bytes and the event loop consumes them.

pub mod keys;
pub mod osc;
pub mod repaint;
pub mod screen;
pub mod shell;
pub mod state;
pub mod terminal;

pub use keys::Key;
pub use osc::{Mark, Marks, Osc133};
pub use screen::{Cursor, ReadResult, Screen};
pub use state::{Exit, InputExpected, InputKind, Source, State};
pub use terminal::{Error, OutputChunk, Terminal, TerminalConfig};
