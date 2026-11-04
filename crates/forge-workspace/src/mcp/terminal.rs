//! The terminal family: the seat's own terminal, driven by bytes and keys.
//!
//! **The agent drives it like a human.** There is no agent mode, no
//! automation flag and no wrapper around the shell: text is typed as bytes,
//! Enter is its own key, Ctrl-C is 0x03, and the shell sees an ordinary
//! `xterm-256color` terminal. Nothing the terminal adds says an agent is
//! behind the keys.
//!
//! **The family is any-caller and it is always on.** The terminal is one per
//! seat on the machine, not a scope like reviews or crons, so every session
//! may drive its own. No approval gate: Bash through the CLI already reaches
//! the shell here. The one thing gated is the person's input, through the
//! hand-off.

use std::sync::Arc;

use forge_primitives::SessionSlot;
use forge_primitives::browser::HandOffEnding;
use forge_sdk::mcp::server::McpServerBuilder;
use forge_sdk::mcp::tool::{Tool, ToolInput, ToolOutput, ToolOutputBlock};
use forge_terminal::Key;
use serde_json::Value;

use crate::mcp::terminal::facade::{
    OpenRequest, TerminalFacade, WaitUntil,
};

pub mod facade;

/// The most lines and bytes one read returns unless the caller asks for less.
const DEFAULT_MAX_LINES: usize = 200;
const DEFAULT_MAX_BYTES: usize = 32 * 1024;
/// The longest a wait or a run may hold before it answers unmatched.
const DEFAULT_TIMEOUT_MS: u64 = 30_000;

/// Attach the terminal tools to an existing builder.
pub(crate) fn add_tools(
    builder: McpServerBuilder,
    facade: Arc<dyn TerminalFacade>,
    slot: SessionSlot,
) -> McpServerBuilder {
    let mut builder = builder;
    for tool in [
        TerminalTool::Open,
        TerminalTool::Write,
        TerminalTool::Read,
        TerminalTool::Wait,
        TerminalTool::Resize,
        TerminalTool::Close,
        TerminalTool::HandOff,
        TerminalTool::Run,
    ] {
        builder = builder.tool(Registered {
            tool,
            facade: Arc::clone(&facade),
            slot: slot.clone(),
        });
    }
    builder
}

/// One tool of the family.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TerminalTool {
    Open,
    Write,
    Read,
    Wait,
    Resize,
    Close,
    HandOff,
    Run,
}

impl TerminalTool {
    const fn name(self) -> &'static str {
        match self {
            Self::Open => "terminal__open",
            Self::Write => "terminal__write",
            Self::Read => "terminal__read",
            Self::Wait => "terminal__wait",
            Self::Resize => "terminal__resize",
            Self::Close => "terminal__close",
            Self::HandOff => "terminal__hand_off",
            Self::Run => "terminal__run",
        }
    }

    const fn description(self) -> &'static str {
        match self {
            Self::Open => {
                "Open this seat's terminal (one per seat, created on first use) and answer its \
                 screen. The terminal is a real PTY: the shell is the person's own, its cwd and \
                 environment are the seat's, and a password prompt, sudo, ssh and a curses \
                 program behave as they do in a normal terminal. **Prefer the CLI's own Bash \
                 tool for a non-interactive command**: it already returns the exit status and \
                 output without a terminal. Use this for the TTY cases - an interactive CLI, a \
                 curses program, a password prompt (hand it to the person with \
                 terminal__hand_off), or a command the person should be able to watch. `cwd` \
                 applies only when the terminal is first created; later opens return the same \
                 terminal. `command` is typed into the shell once it is ready (text, then \
                 Enter). `cols` and `rows` set the size when both are given."
            }
            Self::Write => {
                "Type into this seat's terminal. `text` is typed as-is; `keys` is one or more \
                 named specials pressed in order: enter, tab, escape, up, down, left, right, \
                 home, end, page_up, page_down, backspace, ctrl_c, ctrl_d, ctrl_z, ctrl_l. \
                 Nothing is a command API: the bytes reach the shell as a person's would. \
                 Refused while a hand-off is parked, because the person owns the input then. \
                 Make sure the terminal has finished running a command before typing (see \
                 terminal__wait) - keystrokes typed while a program is busy go to that program."
            }
            Self::Read => {
                "Read this seat's terminal as plain text, never raw escapes. The text is the \
                 scrollback and the visible screen, bounded by `max_lines` (default 200) and \
                 `max_bytes` (default 32 KiB); `truncated` says the bound cut it. `cursor` is \
                 the position to pass as `since` on the next read to get only what is new. \
                 Output is never pushed to you: reading is how you see it. Refused while a \
                 hand-off is parked."
            }
            Self::Wait => {
                "Wait on this seat's terminal. `until` is \"exit\" (the running command \
                 finished; exact through the shell's OSC 133 marks, with the heuristic falling \
                 back to a quiet prompt and `state.source` saying which), \"quiet\" (no output \
                 for the quiet window), or an object {pattern: \"text\"} (the screen contains \
                 it). `timeout_ms` defaults to 30000; `matched` says whether the condition \
                 arrived before it. The answer carries the current screen text and cursor, so \
                 you usually do not need a separate read."
            }
            Self::Resize => {
                "Resize this seat's terminal, in columns and rows. A client's attach resizes it \
                 too; use this when you need a wider screen for a tool. Refused outside 2..=500 \
                 columns and 2..=300 rows."
            }
            Self::Close => {
                "Close this seat's terminal and kill its process group. The seat's next \
                 terminal__open starts a fresh shell in the seat's cwd. **A closed terminal's \
                 scrollback is gone**: read what you need first."
            }
            Self::HandOff => {
                "Hand this seat's terminal to the person at a client for the one thing only they \
                 can do: a password, a sudo confirmation, a Y/N prompt, a device sign-in. The \
                 ask raises the terminal surface and parks, with no timeout, until they answer. \
                 **You are blind while it is parked**: terminal__read and terminal__write are \
                 refused, and their keystrokes never reach you or the transcript. `reason` is \
                 what they should do. The answer is the ending only - done means carry on; \
                 not_now means decide what to do instead; feedback means they sent you a message \
                 about it. Verify the outcome by reading the artifact or the terminal, never by \
                 assuming. `secret: true` on the card means the program is reading with ECHO \
                 off, so their typing is hidden."
            }
            Self::Run => {
                "Run one command in a fresh terminal and answer its exit and text. This is \
                 terminal__open + terminal__wait {until: \"exit\"} + terminal__close with a \
                 guaranteed close on every path, so it is the right call when you just need a \
                 TTY-backed command's result. `command` is typed into the shell once it is \
                 ready. `cwd` defaults to the seat's own. `timeout_ms` defaults to 30000; a \
                 timeout kills the process group and the answer carries signal 9. The text is \
                 bounded and `truncated` says so."
            }
        }
    }

    fn schema(self) -> Value {
        match self {
            Self::Open => serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "A command typed into the shell once it is ready.",
                    },
                    "cwd": {
                        "type": "string",
                        "description": "The cwd the terminal is first spawned in; the seat's own \
                                        when omitted.",
                    },
                    "cols": {"type": "integer", "minimum": 2, "maximum": 500},
                    "rows": {"type": "integer", "minimum": 2, "maximum": 300},
                },
            }),
            Self::Write => serde_json::json!({
                "type": "object",
                "properties": {
                    "id": {"type": "integer", "description": "The terminal id terminal__open returned."},
                    "text": {"type": "string", "description": "Text typed as-is."},
                    "keys": {
                        "type": "array",
                        "items": {
                            "type": "string",
                            "enum": [
                                "enter", "tab", "escape", "up", "down", "left", "right",
                                "home", "end", "page_up", "page_down", "backspace",
                                "ctrl_c", "ctrl_d", "ctrl_z", "ctrl_l",
                            ],
                        },
                        "description": "Named keys pressed in order.",
                    },
                },
                "required": ["id"],
            }),
            Self::Read => serde_json::json!({
                "type": "object",
                "properties": {
                    "id": {"type": "integer", "description": "The terminal id terminal__open returned."},
                    "since": {
                        "type": "integer",
                        "description": "The cursor a previous read returned; only lines after it \
                                        are read.",
                    },
                    "max_lines": {"type": "integer", "minimum": 1, "default": DEFAULT_MAX_LINES},
                    "max_bytes": {"type": "integer", "minimum": 1, "default": DEFAULT_MAX_BYTES},
                },
                "required": ["id"],
            }),
            Self::Wait => serde_json::json!({
                "type": "object",
                "properties": {
                    "id": {"type": "integer", "description": "The terminal id terminal__open returned."},
                    "until": {
                        "description": "What to wait for: \"exit\", \"quiet\", or {pattern: \"text\"}.",
                        "anyOf": [
                            {"type": "string", "enum": ["exit", "quiet"]},
                            {
                                "type": "object",
                                "properties": {"pattern": {"type": "string"}},
                                "required": ["pattern"],
                            },
                        ],
                    },
                    "timeout_ms": {"type": "integer", "minimum": 1, "default": DEFAULT_TIMEOUT_MS},
                },
                "required": ["id", "until"],
            }),
            Self::Resize => serde_json::json!({
                "type": "object",
                "properties": {
                    "id": {"type": "integer", "description": "The terminal id terminal__open returned."},
                    "cols": {"type": "integer", "minimum": 2, "maximum": 500},
                    "rows": {"type": "integer", "minimum": 2, "maximum": 300},
                },
                "required": ["id", "cols", "rows"],
            }),
            Self::Close => serde_json::json!({
                "type": "object",
                "properties": {
                    "id": {"type": "integer", "description": "The terminal id terminal__open returned."},
                },
                "required": ["id"],
            }),
            Self::HandOff => serde_json::json!({
                "type": "object",
                "properties": {
                    "id": {"type": "integer", "description": "The terminal id terminal__open returned."},
                    "reason": {
                        "type": "string",
                        "description": "What the person should do, in your own words.",
                    },
                },
                "required": ["id", "reason"],
            }),
            Self::Run => serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "The command to run."},
                    "cwd": {
                        "type": "string",
                        "description": "The cwd to run in; the seat's own when omitted.",
                    },
                    "timeout_ms": {"type": "integer", "minimum": 1, "default": DEFAULT_TIMEOUT_MS},
                },
                "required": ["command"],
            }),
        }
    }
}

/// One registered tool, plus the caller it acts for.
struct Registered {
    tool: TerminalTool,
    facade: Arc<dyn TerminalFacade>,
    slot: SessionSlot,
}

#[async_trait::async_trait]
impl Tool for Registered {
    fn name(&self) -> &'static str {
        self.tool.name()
    }

    fn description(&self) -> &'static str {
        self.tool.description()
    }

    fn input_schema(&self) -> Value {
        self.tool.schema()
    }

    async fn call(&self, input: ToolInput) -> ToolOutput {
        let started = std::time::Instant::now();
        let outcome = self.dispatch(input.value).await;
        tracing::debug!(
            target: "forge_workspace::terminal",
            event_name = "terminal_call",
            tool = self.tool.name(),
            slot = %self.slot.display(),
            ok = outcome.is_ok(),
            ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            "a terminal tool call returned",
        );
        match outcome {
            Ok(text) => ToolOutput {
                blocks: vec![ToolOutputBlock::Text { text }],
                is_error: false,
            },
            Err(why) => ToolOutput::error(why),
        }
    }
}

impl Registered {
    async fn dispatch(&self, args: Value) -> Result<String, String> {
        match self.tool {
            TerminalTool::Open => self.open(args).await,
            TerminalTool::Write => self.write(args).await,
            TerminalTool::Read => self.read(args).await,
            TerminalTool::Wait => self.wait(args).await,
            TerminalTool::Resize => self.resize(args).await,
            TerminalTool::Close => self.close(args).await,
            TerminalTool::HandOff => self.hand_off(args).await,
            TerminalTool::Run => self.run(args).await,
        }
    }

    async fn open(&self, args: Value) -> Result<String, String> {
        let request = OpenRequest {
            command: optional_string(&args, "command")?,
            cwd: optional_string(&args, "cwd")?,
            cols: optional_u16(&args, "cols")?,
            rows: optional_u16(&args, "rows")?,
        };
        let answer = self.facade.open(&self.slot, request).await?;
        Ok(serde_json::json!({
            "id": answer.id,
            "screen": answer.screen,
            "cols": answer.cols,
            "rows": answer.rows,
            "shell": answer.shell,
            "cwd": answer.cwd,
        })
        .to_string())
    }

    async fn write(&self, args: Value) -> Result<String, String> {
        let id = required_u16(&args, "id")?;
        let text = optional_string(&args, "text")?;
        let keys = parse_keys(&args)?;
        if text.as_deref().is_none_or(str::is_empty) && keys.is_empty() {
            return Err("terminal__write needs `text`, `keys`, or both".to_owned());
        }
        let state = self.facade.write(&self.slot, id, text, keys).await?;
        Ok(serde_json::json!({ "ok": true, "state": state }).to_string())
    }

    async fn read(&self, args: Value) -> Result<String, String> {
        let id = required_u16(&args, "id")?;
        let since = match args.get("since") {
            None | Some(Value::Null) => None,
            Some(value) => Some(
                value
                    .as_u64()
                    .ok_or_else(|| "terminal__read `since` is the cursor a previous read returned, as an integer".to_owned())?,
            ),
        };
        let max_lines = optional_usize(&args, "max_lines")?.unwrap_or(DEFAULT_MAX_LINES);
        let max_bytes = optional_usize(&args, "max_bytes")?.unwrap_or(DEFAULT_MAX_BYTES);
        let answer = self.facade.read(&self.slot, id, since, max_lines, max_bytes).await?;
        Ok(serde_json::json!({
            "text": answer.text,
            "cursor": answer.cursor,
            "cols": answer.cols,
            "rows": answer.rows,
            "state": answer.state,
            "truncated": answer.truncated,
        })
        .to_string())
    }

    async fn wait(&self, args: Value) -> Result<String, String> {
        let id = required_u16(&args, "id")?;
        let until = parse_until(&args)?;
        let timeout_ms = optional_u64(&args, "timeout_ms")?.unwrap_or(DEFAULT_TIMEOUT_MS);
        let answer = self.facade.wait(&self.slot, id, until, timeout_ms).await?;
        Ok(serde_json::json!({
            "matched": answer.matched,
            "text": answer.text,
            "cursor": answer.cursor,
            "state": answer.state,
            "exited": answer.state.exit,
        })
        .to_string())
    }

    async fn resize(&self, args: Value) -> Result<String, String> {
        let id = required_u16(&args, "id")?;
        let cols = required_u16(&args, "cols")?;
        let rows = required_u16(&args, "rows")?;
        if !(forge_primitives::terminal::MIN_COLS..=forge_primitives::terminal::MAX_COLS)
            .contains(&cols)
            || !(forge_primitives::terminal::MIN_ROWS..=forge_primitives::terminal::MAX_ROWS)
                .contains(&rows)
        {
            return Err(format!(
                "terminal__resize takes {min_cols}..={max_cols} columns and {min_rows}..={max_rows} \
                 rows",
                min_cols = forge_primitives::terminal::MIN_COLS,
                max_cols = forge_primitives::terminal::MAX_COLS,
                min_rows = forge_primitives::terminal::MIN_ROWS,
                max_rows = forge_primitives::terminal::MAX_ROWS,
            ));
        }
        self.facade.resize(&self.slot, id, cols, rows).await?;
        Ok(serde_json::json!({ "ok": true }).to_string())
    }

    async fn close(&self, args: Value) -> Result<String, String> {
        let id = required_u16(&args, "id")?;
        let exit = self.facade.close(&self.slot, id).await?;
        Ok(serde_json::json!({ "ok": true, "exit": exit }).to_string())
    }

    async fn hand_off(&self, args: Value) -> Result<String, String> {
        let id = required_u16(&args, "id")?;
        let Some(reason) = optional_string(&args, "reason")?
            .filter(|reason| !reason.trim().is_empty())
        else {
            return Err("terminal__hand_off needs `reason`: what the person should do".to_owned());
        };
        match self.facade.hand_off(&self.slot, id, &reason).await? {
            HandOffEnding::Done => Ok(
                "The person is done in the terminal; carry on from where you left off. Read the \
                 terminal or the artifact to verify the outcome."
                    .to_string(),
            ),
            HandOffEnding::NotNow => Err(
                "The person declined to act for now; the terminal is as it was - decide what to \
                 do instead."
                    .to_string(),
            ),
            HandOffEnding::Feedback => Ok(
                "The person sent you a message about the terminal instead of Done or Not now; \
                 read it and carry on - the terminal is as it was."
                    .to_string(),
            ),
            HandOffEnding::Abandoned => Err("The hand-off ended without an answer.".to_string()),
        }
    }

    async fn run(&self, args: Value) -> Result<String, String> {
        let Some(command) = optional_string(&args, "command")?
            .filter(|command| !command.trim().is_empty())
        else {
            return Err("terminal__run needs `command`: the command to run".to_owned());
        };
        let cwd = optional_string(&args, "cwd")?;
        let timeout_ms = optional_u64(&args, "timeout_ms")?.unwrap_or(DEFAULT_TIMEOUT_MS);
        let answer = self.facade.run(&self.slot, &command, cwd, timeout_ms).await?;
        Ok(serde_json::json!({
            "exit": answer.exit,
            "text": answer.text,
            "truncated": answer.truncated,
        })
        .to_string())
    }
}

fn required_u16(args: &Value, key: &str) -> Result<u16, String> {
    let value = args
        .get(key)
        .ok_or_else(|| format!("this call needs `{key}`, the terminal id terminal__open returned"))?;
    u16::try_from(value.as_u64().ok_or_else(|| format!("`{key}` must be an integer"))?)
        .map_err(|_| format!("`{key}` is out of range"))
}

fn optional_u16(args: &Value, key: &str) -> Result<Option<u16>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => Ok(Some(
            u16::try_from(value.as_u64().ok_or_else(|| format!("`{key}` must be an integer"))?)
                .map_err(|_| format!("`{key}` is out of range"))?,
        )),
    }
}

fn optional_u64(args: &Value, key: &str) -> Result<Option<u64>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => {
            Ok(Some(value.as_u64().ok_or_else(|| format!("`{key}` must be an integer"))?))
        }
    }
}

fn optional_usize(args: &Value, key: &str) -> Result<Option<usize>, String> {
    Ok(optional_u64(args, key)?.map(|value| usize::try_from(value).unwrap_or(usize::MAX)))
}

fn optional_string(args: &Value, key: &str) -> Result<Option<String>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(format!("`{key}` must be a string")),
    }
}

fn parse_keys(args: &Value) -> Result<Vec<Key>, String> {
    let Some(keys) = args.get("keys") else { return Ok(Vec::new()) };
    if keys.is_null() {
        return Ok(Vec::new());
    }
    let Some(keys) = keys.as_array() else {
        return Err("terminal__write `keys` is an array of key names".to_owned());
    };
    let mut parsed = Vec::with_capacity(keys.len());
    for key in keys {
        let Some(name) = key.as_str() else {
            return Err("terminal__write `keys` is an array of key names".to_owned());
        };
        let Some(key) = Key::parse(name) else {
            return Err(format!(
                "'{name}' is not a key this tool takes: enter, tab, escape, up, down, left, \
                 right, home, end, page_up, page_down, backspace, ctrl_c, ctrl_d, ctrl_z, ctrl_l"
            ));
        };
        parsed.push(key);
    }
    Ok(parsed)
}

fn parse_until(args: &Value) -> Result<WaitUntil, String> {
    let Some(until) = args.get("until") else {
        return Err("terminal__wait needs `until`: \"exit\", \"quiet\", or {pattern: \"text\"}"
            .to_owned());
    };
    match until {
        Value::String(word) if word == "exit" => Ok(WaitUntil::Exit),
        Value::String(word) if word == "quiet" => Ok(WaitUntil::Quiet),
        Value::String(word) => Err(format!(
            "'{word}' is not an `until` this tool takes: \"exit\", \"quiet\", or {{pattern: \
             \"text\"}}"
        )),
        Value::Object(map) => {
            let Some(pattern) = map.get("pattern").and_then(Value::as_str) else {
                return Err("terminal__wait {pattern: ...} needs a non-empty string".to_owned());
            };
            if pattern.is_empty() {
                return Err("terminal__wait {pattern: ...} needs a non-empty string".to_owned());
            }
            Ok(WaitUntil::Pattern(pattern.to_owned()))
        }
        _ => Err(
            "terminal__wait needs `until`: \"exit\", \"quiet\", or {pattern: \"text\"}".to_owned(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::terminal::facade::MockTerminalFacade;
    use serde_json::json;

    fn seat() -> SessionSlot {
        SessionSlot::from_str_for_test("caller")
    }

    fn tool(name: &str, facade: Arc<dyn TerminalFacade>) -> Registered {
        let tool = [
            TerminalTool::Open,
            TerminalTool::Write,
            TerminalTool::Read,
            TerminalTool::Wait,
            TerminalTool::Resize,
            TerminalTool::Close,
            TerminalTool::HandOff,
            TerminalTool::Run,
        ]
        .into_iter()
        .find(|tool| tool.name() == name)
        .unwrap_or_else(|| panic!("{name} is not on the surface"));
        Registered { tool, facade, slot: seat() }
    }

    fn text(out: &ToolOutput) -> String {
        crate::mcp::test_support::block_text(&out.blocks[0]).to_owned()
    }

    /// **The family is the eight tools the spec names.** A missing one is a
    /// prompt that does not resolve; an extra one is a surface no test drives.
    #[test]
    fn the_family_is_the_eight_tools() {
        let names: Vec<&str> = [
            TerminalTool::Open,
            TerminalTool::Write,
            TerminalTool::Read,
            TerminalTool::Wait,
            TerminalTool::Resize,
            TerminalTool::Close,
            TerminalTool::HandOff,
            TerminalTool::Run,
        ]
        .iter()
        .map(|tool| tool.name())
        .collect();
        assert_eq!(
            names,
            [
                "terminal__open",
                "terminal__write",
                "terminal__read",
                "terminal__wait",
                "terminal__resize",
                "terminal__close",
                "terminal__hand_off",
                "terminal__run",
            ],
        );
        for name in &names {
            assert!(name.starts_with("terminal__"), "{name} is under the terminal family");
        }
    }

    /// A write carries the text and the named keys, parsed to their bytes,
    /// and answers the state.
    #[tokio::test]
    async fn a_write_carries_the_text_and_the_keys() {
        let mock = Arc::new(MockTerminalFacade::new());
        let out = tool("terminal__write", Arc::clone(&mock) as Arc<dyn TerminalFacade>)
            .call(ToolInput { value: json!({ "id": 1, "text": "ls", "keys": ["enter"] }) })
            .await;
        assert!(!out.is_error, "{:?}", out.blocks);
        assert_eq!(
            mock.writes.lock().as_slice(),
            [(1, Some("ls".to_owned()), vec![Key::Enter])],
            "the text and the key cross to the terminal",
        );
    }

    /// A call with neither text nor keys is refused rather than answered with
    /// nothing done.
    #[tokio::test]
    async fn a_write_with_nothing_to_type_is_refused() {
        let mock = Arc::new(MockTerminalFacade::new());
        let out = tool("terminal__write", Arc::clone(&mock) as Arc<dyn TerminalFacade>)
            .call(ToolInput { value: json!({ "id": 1 }) })
            .await;
        assert!(out.is_error);
        assert!(text(&out).contains("`text`"), "{}", text(&out));
        assert!(mock.writes.lock().is_empty());
    }

    /// An unknown key name is refused naming the key and the set, so a model
    /// cannot quietly lose one key of a sequence.
    #[tokio::test]
    async fn an_unknown_key_is_refused_by_name() {
        let mock = Arc::new(MockTerminalFacade::new());
        let out = tool("terminal__write", Arc::clone(&mock) as Arc<dyn TerminalFacade>)
            .call(ToolInput { value: json!({ "id": 1, "keys": ["ctrl_q"] }) })
            .await;
        assert!(out.is_error);
        assert!(text(&out).contains("ctrl_q"), "{}", text(&out));
        assert!(text(&out).contains("ctrl_c"), "the set is named: {}", text(&out));
    }

    /// A hand-off parks on the person and a done answers it with the words a
    /// model can act on.
    #[tokio::test]
    async fn a_hand_off_parks_and_a_done_answers_it() {
        let mock = Arc::new(MockTerminalFacade::new());
        let out = tool("terminal__hand_off", Arc::clone(&mock) as Arc<dyn TerminalFacade>)
            .call(ToolInput { value: json!({ "id": 2, "reason": "sudo password" }) })
            .await;
        assert!(!out.is_error, "{:?}", out.blocks);
        assert_eq!(
            mock.hand_offs.lock().as_slice(),
            [(2, "sudo password".to_owned())],
            "the reason and the terminal cross to the park",
        );
        assert!(text(&out).contains("carry on"), "{}", text(&out));
    }

    /// A decline is a tool error, so the model reads it as "not this way".
    #[tokio::test]
    async fn a_hand_off_decline_is_a_tool_error() {
        let mock = Arc::new(MockTerminalFacade::new());
        *mock.hand_off_answer.lock() = Ok(HandOffEnding::NotNow);
        let out = tool("terminal__hand_off", Arc::clone(&mock) as Arc<dyn TerminalFacade>)
            .call(ToolInput { value: json!({ "id": 2, "reason": "sign in" }) })
            .await;
        assert!(out.is_error);
        assert!(text(&out).contains("declined"), "{}", text(&out));
    }

    /// A blank reason is no ask: it would park a prompt that says nothing.
    #[tokio::test]
    async fn a_hand_off_without_a_reason_is_refused() {
        let mock = Arc::new(MockTerminalFacade::new());
        let out = tool("terminal__hand_off", Arc::clone(&mock) as Arc<dyn TerminalFacade>)
            .call(ToolInput { value: json!({ "id": 2, "reason": "  " }) })
            .await;
        assert!(out.is_error);
        assert!(text(&out).contains("`reason`"), "{}", text(&out));
        assert!(mock.hand_offs.lock().is_empty());
    }

    /// The wait parses all three `until` shapes and passes the default
    /// timeout.
    #[tokio::test]
    async fn a_wait_parses_all_three_until_shapes() {
        let mock = Arc::new(MockTerminalFacade::new());
        let wait = |value: Value| {
            let mock = Arc::clone(&mock);
            async move {
                tool("terminal__wait", mock as Arc<dyn TerminalFacade>)
                    .call(ToolInput { value })
                    .await
            }
        };
        let _ = wait(json!({ "id": 1, "until": "exit" })).await;
        let _ = wait(json!({ "id": 1, "until": "quiet" })).await;
        let _ = wait(json!({ "id": 1, "until": { "pattern": "done" } })).await;
        let seen = mock.waits.lock().clone();
        assert_eq!(
            seen,
            vec![
                (1, WaitUntil::Exit, DEFAULT_TIMEOUT_MS),
                (1, WaitUntil::Quiet, DEFAULT_TIMEOUT_MS),
                (1, WaitUntil::Pattern("done".to_owned()), DEFAULT_TIMEOUT_MS),
            ],
        );
    }

    /// A resize outside the sane bounds is refused before it reaches the PTY.
    #[tokio::test]
    async fn a_resize_outside_the_bounds_is_refused() {
        let mock = Arc::new(MockTerminalFacade::new());
        for (cols, rows) in [(1, 24), (501, 24), (80, 1), (80, 301)] {
            let out = tool("terminal__resize", Arc::clone(&mock) as Arc<dyn TerminalFacade>)
                .call(ToolInput { value: json!({ "id": 1, "cols": cols, "rows": rows }) })
                .await;
            assert!(out.is_error, "{cols}x{rows}: {:?}", out.blocks);
        }
        assert!(mock.resizes.lock().is_empty());
    }

    /// A run carries the command and the cwd, and answers the exit and text.
    #[tokio::test]
    async fn a_run_carries_the_command_and_answers_the_exit() {
        let mock = Arc::new(MockTerminalFacade::new());
        let out = tool("terminal__run", Arc::clone(&mock) as Arc<dyn TerminalFacade>)
            .call(ToolInput {
                value: json!({ "command": "echo hi", "cwd": "/tmp", "timeout_ms": 1000 }),
            })
            .await;
        assert!(!out.is_error, "{:?}", out.blocks);
        assert_eq!(
            mock.runs.lock().as_slice(),
            [("echo hi".to_owned(), Some("/tmp".to_owned()), 1000)],
        );
        let answer: Value = serde_json::from_str(&text(&out)).expect("a JSON answer");
        assert_eq!(answer["exit"]["code"], 0);
        assert_eq!(answer["text"], "ran");
    }

    /// A run without a command is the call's own mistake.
    #[tokio::test]
    async fn a_run_without_a_command_is_refused() {
        let mock = Arc::new(MockTerminalFacade::new());
        let out = tool("terminal__run", Arc::clone(&mock) as Arc<dyn TerminalFacade>)
            .call(ToolInput { value: json!({}) })
            .await;
        assert!(out.is_error);
        assert!(text(&out).contains("`command`"), "{}", text(&out));
    }

    /// The read's bound and cursor cross to the facade and back, so a model
    /// paging through output gets the shape the tool advertises.
    #[tokio::test]
    async fn a_read_carries_the_bound_and_the_cursor() {
        let mock = Arc::new(MockTerminalFacade::new());
        let out = tool("terminal__read", Arc::clone(&mock) as Arc<dyn TerminalFacade>)
            .call(ToolInput {
                value: json!({ "id": 1, "since": 4, "max_lines": 10, "max_bytes": 100 }),
            })
            .await;
        assert!(!out.is_error, "{:?}", out.blocks);
        assert_eq!(mock.reads.lock().as_slice(), [(1, Some(4), 10, 100)]);
        let answer: Value = serde_json::from_str(&text(&out)).expect("a JSON answer");
        assert_eq!(answer["cursor"], 1);
        assert_eq!(answer["truncated"], false);
    }
}
