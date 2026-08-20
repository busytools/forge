//! Session-persistence → stream-json transformer + PII redactor.
//!
//! Claude Code persists each session as JSONL under
//! `$CLAUDE_CONFIG_DIR/projects/<slug>/<session-id>.jsonl`, overlapping
//! the wire protocol in broad shape but adding camelCase
//! persistence-only fields and dropping wire-only frames.
//! [`transform_persistence_line`] converts one such line to wire shape;
//! both it and [`WireRedactor`] then redact.
//!
//! Most rules are SPELLING rules - home paths, `.claude-<profile>`
//! directories (`.claude-plugin` excepted, it is a plugin manifest),
//! `-Users-<name>-` project slugs, and the home-directory owner as a bare
//! word. Those recognise nothing by meaning, so a check written from them
//! can only confirm they ran, and **prose in a captured tool result is
//! not redacted at all**. Read a new capture before committing it.
//!
//! Four rules are instead value-blind, replacing a field wholesale
//! rather than rewriting spellings inside it: everything non-categorical
//! under `account`, the body of a `hook_response` frame, the command /
//! skill inventory, and an `mcpServers[].config` blob. Each is a field
//! where no spelling rule could ever be enough - a hook is an arbitrary
//! local program, and its output on this repo's own captures was the
//! author's entire cross-project memory index, in prose, which every
//! spelling rule passed through untouched.
//!
//! Prefer that shape when adding a rule. A fixed-point gate over this
//! module can only ever check that a capture agrees with these rules, so
//! its reach is bounded by them: it is strengthened by a rule that
//! recognises nothing, never by a longer list of things to recognise.
//!
//! Two rules are entry-point specific: the owner rule needs a whole
//! trace to discover the name, so only [`WireRedactor`] has it; message
//! bodies, tool inputs and results are stubbed only by
//! [`transform_persistence_line`], which also drops the
//! persistence-only fields rather than tokenising them.
//!
//! Deterministic: one input line always gives the same output line.

use std::collections::HashMap;
use std::sync::OnceLock;

use serde_json::Value;

/// Home-path prefixes, replaced by `<redacted-home>` with the path tail
/// kept. The classes must exclude `"`: these patterns also run over raw
/// line text, where a class that swallows the delimiter eats the field
/// that follows. That is also what keeps the `,` they DO match harmless -
/// drop the `"` again and the comma inclusion becomes a field-eater.
fn path_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r#"(/Users/[^/\s\\"]+|/home/[^/\s\\"]+|/Volumes/[^/\s\\"]+)"#)
            .expect("redactor path regex must compile")
    })
}

/// A `claude` profile directory name, which [`path_regex`] cannot reach:
/// it replaces only the home prefix, and `~/.claude-alt` has no prefix at
/// all. Bare `.claude` is the default config dir, so it is not matched.
fn profile_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"\.claude-[A-Za-z0-9._-]+").expect("redactor profile regex must compile")
    })
}

/// The project-slug spelling of a home path, which Claude Code mints by
/// turning `/` into `-` (`-Users-<name>-Projects-forge`). Captures the
/// name so discovery can harvest it as well as redact it.
fn slug_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r#"-Users-([^-/\s\\"]+)-"#).expect("redactor slug regex must compile")
    })
}

/// A plugin manifest directory, not a profile - [`profile_regex`] would
/// otherwise take it for one.
const PLUGIN_MANIFEST_DIR: &str = ".claude-plugin";

/// Shortest owner name scrubbed as a bare token; a shorter one is an
/// error, not a skip. This does not make token scrubbing safe -
/// `assistant` and `permission` clear it - but a collision above the
/// floor fails loudly on replay, and below it would not.
const MIN_OWNER_LEN: usize = 8;

/// True when any string-level rule would rewrite `s`.
fn needs_scrub(s: &str) -> bool {
    path_regex().is_match(s) || profile_regex().is_match(s) || slug_regex().is_match(s)
}

/// Apply every string-level rule to `s`. Ordered widest-first so an
/// absolute path loses its home prefix before its profile segment.
fn scrub_text(s: &str) -> String {
    let out = path_regex().replace_all(s, "<redacted-home>");
    // The regex crate has no lookahead, so the plugin dir is excluded
    // by the replacement rather than by the pattern.
    let out = profile_regex().replace_all(&out, |caps: &regex::Captures| {
        if &caps[0] == PLUGIN_MANIFEST_DIR {
            PLUGIN_MANIFEST_DIR.to_string()
        } else {
            "<redacted-profile>".to_string()
        }
    });
    slug_regex().replace_all(&out, "<redacted-home>-").into_owned()
}

/// Captures the home-directory OWNER name out of an absolute home path.
/// Narrower than [`path_regex`] in two ways: no `/Volumes/` arm, because
/// a volume name is not a user name, and a user-name character class,
/// because stderr puts `:` or a quote straight after the path and a
/// captured fragment then matches nothing.
fn owner_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"/(?:Users|home)/([A-Za-z0-9._-]+)")
            .expect("redactor owner regex must compile")
    })
}

/// Account keys whose value is a categorical CLI label, not personal
/// data: the corpus records two distinct `subscriptionType` labels and
/// collapsing them would delete a wire fact the fixture exists to
/// record. Only keys observed nested under an account object belong
/// here - an exemption that never fires just risks leaving a real value.
const ACCOUNT_CATEGORICAL_KEYS: &[&str] = &["apiProvider", "subscriptionType"];

/// Placeholder every non-categorical string under `account` /
/// `accounts`, keeping the keys so the fixture still records the CLI's
/// shape. Structural rather than value-matching, so a field the CLI adds
/// later is redacted before anyone notices it leaked.
fn redact_account_recursive(v: &mut Value) {
    match v {
        Value::Array(a) => a.iter_mut().for_each(redact_account_recursive),
        Value::Object(o) => {
            for (key, val) in o.iter_mut() {
                if key == "account" || key == "accounts" {
                    redact_account_field(val, key);
                } else {
                    redact_account_recursive(val);
                }
            }
        }
        _ => {}
    }
}

/// Redact one value under an account field. `key` names whatever it hung
/// off, so a bare `"account": "…"` and a nested
/// `"account": {"profile": {"email": …}}` both get a naming placeholder.
fn redact_account_field(v: &mut Value, key: &str) {
    match v {
        Value::String(s) if !ACCOUNT_CATEGORICAL_KEYS.contains(&key) => {
            let placeholder = format!("<redacted-{key}>");
            if *s != placeholder {
                // `tool_use.input` is not stubbed on the wire path, so an
                // MCP tool taking an `account` argument has that argument
                // collapsed and the fixture then records a wire fact that
                // never happened. Log so a fixture-regen review sees it.
                eprintln!("redact_account_field: collapsed a value under key={key:?}");
                *s = placeholder;
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|e| redact_account_field(e, key)),
        Value::Object(o) => {
            for (k, val) in o.iter_mut() {
                redact_account_field(val, k);
            }
        }
        _ => {}
    }
}

/// Body fields of a `hook_response` frame. A hook is an arbitrary local
/// program, so these carry whatever the capture machine happened to
/// print, which on this repo's own captures was the author's entire
/// cross-project memory index.
const HOOK_BODY_KEYS: &[&str] = &["output", "stdout", "stderr"];

/// What replaces a hook body. Non-empty, so a capture cannot pass the
/// fixed-point gate by having merely been emptied by hand.
const HOOK_BODY_PLACEHOLDER: &str = "<redacted-hook-body>";

/// Stub the body of every `hook_response` frame, keeping the keys and
/// every other field so the fixture still records the CLI's shape.
///
/// Value-blind by construction: the spelling rules elsewhere in this file
/// are a no-op on prose, so no amount of pattern-matching makes a hook
/// body safe to commit. The frame is scoped by its own `type` + `subtype`
/// rather than by key name, because `stdout` is also how a Bash tool
/// result reports itself and that one is wire surface worth keeping.
///
/// An empty body is left alone: it cannot carry anything, and a quiet
/// hook is a wire fact the fixture should still record.
fn redact_hook_body_recursive(v: &mut Value) {
    match v {
        Value::Array(a) => a.iter_mut().for_each(redact_hook_body_recursive),
        Value::Object(o) => {
            if o.get("type").and_then(Value::as_str) == Some("system")
                && o.get("subtype").and_then(Value::as_str) == Some("hook_response")
            {
                for key in HOOK_BODY_KEYS {
                    if let Some(Value::String(body)) = o.get_mut(*key)
                        && !body.is_empty()
                        && body != HOOK_BODY_PLACEHOLDER
                    {
                        HOOK_BODY_PLACEHOLDER.clone_into(body);
                    }
                }
            }
            for val in o.values_mut() {
                redact_hook_body_recursive(val);
            }
        }
        _ => {}
    }
}

/// Per-entry fields of a command inventory entry. `argumentHint` is only
/// stubbed when already present and non-empty: the TUI collapses an empty
/// hint to `None`, so inventing one would record a wire fact that never
/// happened.
const COMMAND_ENTRY_KEYS: &[&str] = &["description", "argumentHint"];

/// Stub the command / skill inventory the CLI reports for whatever is
/// installed on the capture machine: `slash_commands` and `skills` on
/// `system/init`, `commands` on `system/commands_changed`, and the
/// `commands` reply carried by a `control_response`.
///
/// Both halves go, not just the descriptions. A description is local
/// prose; a name is a local project's name, and this repo's own captures
/// carried a work project in both. Nothing in forge matches on a specific
/// command name - the TUI's parser takes any non-empty string and its
/// lookup compares against what the user typed - so a placeholder keeps
/// every wire fact the fixture records.
///
/// Placeholders are index-suffixed to keep the list's length and the
/// entries distinct, which a single shared placeholder would collapse.
fn redact_command_inventory_recursive(v: &mut Value) {
    match v {
        Value::Array(a) => a.iter_mut().for_each(redact_command_inventory_recursive),
        Value::Object(o) => {
            let is_system = o.get("type").and_then(Value::as_str) == Some("system");
            let is_init = is_system && o.get("subtype").and_then(Value::as_str) == Some("init");
            let is_changed =
                is_system && o.get("subtype").and_then(Value::as_str) == Some("commands_changed");
            let is_control = o.get("type").and_then(Value::as_str) == Some("control_response");
            if is_init {
                for (key, kind) in [("slash_commands", "command"), ("skills", "skill")] {
                    if let Some(val) = o.get_mut(key) {
                        stub_inventory(val, kind);
                    }
                }
            }
            if is_changed && let Some(val) = o.get_mut("commands") {
                stub_inventory(val, "command");
            }
            if is_control {
                for val in o.values_mut() {
                    stub_nested_commands(val);
                }
                return;
            }
            for val in o.values_mut() {
                redact_command_inventory_recursive(val);
            }
        }
        _ => {}
    }
}

/// Inventory arrays a `control_response` can carry, and what each holds:
/// `commands` from the command-list reply, `skillFrontmatter` from the
/// context-usage reply. Both are keyed inside that frame rather than
/// globally, which is what keeps a same-named field elsewhere on the wire
/// out of scope.
const CONTROL_INVENTORY_KEYS: &[(&str, &str)] =
    &[("commands", "command"), ("skillFrontmatter", "skill")];

/// Find every inventory array under a `control_response` and stub it.
/// Scoped to that frame by the caller.
fn stub_nested_commands(v: &mut Value) {
    match v {
        Value::Array(a) => a.iter_mut().for_each(stub_nested_commands),
        Value::Object(o) => {
            for (key, val) in o.iter_mut() {
                match CONTROL_INVENTORY_KEYS.iter().find(|(k, _)| *k == key.as_str()) {
                    Some((_, kind)) => stub_inventory(val, kind),
                    None => stub_nested_commands(val),
                }
            }
        }
        _ => {}
    }
}

/// Replace each entry of one inventory array, whichever of the two shapes
/// it uses: a bare name string, or an object carrying a name plus local
/// prose about it. A non-array is left alone.
fn stub_inventory(arr: &mut Value, kind: &str) {
    let Value::Array(entries) = arr else { return };
    for (i, entry) in entries.iter_mut().enumerate() {
        let name = format!("<redacted-{kind}-{i}>");
        match entry {
            Value::String(s) => *s = name,
            Value::Object(o) => {
                if let Some(Value::String(s)) = o.get_mut("name") {
                    *s = name;
                }
                for key in COMMAND_ENTRY_KEYS {
                    if let Some(Value::String(s)) = o.get_mut(*key)
                        && !s.is_empty()
                    {
                        *s = format!("<redacted-{kind}-{key}>");
                    }
                }
            }
            _ => {}
        }
    }
}

/// Keys inside an `mcpServers[].config` blob whose value is a CLI label
/// rather than local configuration. `type` is the transport discriminant
/// (`stdio` / `http` / `sse` / `claudeai-proxy`), which the fixture exists
/// to record.
const MCP_CONFIG_CATEGORICAL_KEYS: &[&str] = &["type"];

/// What replaces a value inside an `mcpServers[].config` blob.
const MCP_CONFIG_PLACEHOLDER: &str = "<redacted-mcp-config>";

/// Stub the `config` blob on every `mcpServers[]` entry, keeping the keys
/// and every sibling field.
///
/// `McpServerStatus::config` is decoded as an opaque `serde_json::Value`
/// and nothing in forge reads inside it, so the only wire fact here is
/// the shape. What it actually holds is the capture machine's own MCP
/// configuration: on this repo's captures, a LAN address, an API-key
/// environment variable, and a server id issued by Anthropic's MCP proxy.
/// `name`, `status`, `scope`, `error` and `tools` are untouched - those
/// are what the CLI reports and what forge renders.
fn redact_mcp_config_recursive(v: &mut Value) {
    match v {
        Value::Array(a) => a.iter_mut().for_each(redact_mcp_config_recursive),
        Value::Object(o) => {
            if let Some(Value::Array(servers)) = o.get_mut("mcpServers") {
                for server in servers.iter_mut() {
                    if let Value::Object(fields) = server
                        && let Some(config) = fields.get_mut("config")
                    {
                        stub_mcp_config(config);
                    }
                }
            }
            for val in o.values_mut() {
                redact_mcp_config_recursive(val);
            }
        }
        _ => {}
    }
}

/// Replace every non-categorical string inside one config blob, at any
/// depth, keeping array lengths and object keys.
fn stub_mcp_config(v: &mut Value) {
    match v {
        Value::String(s) => {
            if s != MCP_CONFIG_PLACEHOLDER {
                MCP_CONFIG_PLACEHOLDER.clone_into(s);
            }
        }
        Value::Array(a) => a.iter_mut().for_each(stub_mcp_config),
        Value::Object(o) => {
            for (key, val) in o.iter_mut() {
                if !MCP_CONFIG_CATEGORICAL_KEYS.contains(&key.as_str()) {
                    stub_mcp_config(val);
                }
            }
        }
        _ => {}
    }
}

/// Replace each discovered owner name with `<redacted-user>` in string
/// VALUES only. Keys are left alone: an owner name is never a wire key,
/// and rewriting one would change the shape rather than a value.
fn scrub_owner_tokens_recursive(v: &mut Value, owners: &[regex::Regex]) {
    match v {
        Value::String(s) => *s = replace_owners(s, owners),
        Value::Array(a) => a.iter_mut().for_each(|e| scrub_owner_tokens_recursive(e, owners)),
        Value::Object(o) => {
            for val in o.values_mut() {
                scrub_owner_tokens_recursive(val, owners);
            }
        }
        _ => {}
    }
}

/// Apply every owner pattern to `s`.
fn replace_owners(s: &str, owners: &[regex::Regex]) -> String {
    owners
        .iter()
        .fold(s.to_string(), |acc, owner| owner.replace_all(&acc, "<redacted-user>").into_owned())
}

/// Collect owner names from both spellings of a home path in `s`: the
/// path and the project slug.
///
/// # Errors
///
/// A name shorter than `MIN_OWNER_LEN`.
fn collect_owners(s: &str, out: &mut Vec<String>) -> Result<(), String> {
    let from_paths = owner_regex().captures_iter(s);
    let from_slugs = slug_regex().captures_iter(s);
    for caps in from_paths.chain(from_slugs) {
        let name = caps[1].to_string();
        if name.len() < MIN_OWNER_LEN {
            return Err(format!(
                "discovered a {}-character home-directory owner, below this \
                 redactor's {MIN_OWNER_LEN}-character floor",
                name.len()
            ));
        }
        if !out.contains(&name) {
            out.push(name);
        }
    }
    Ok(())
}

/// Collect owner names from every string a parsed line holds. Reading
/// parsed values rather than raw text matters: over raw text
/// `{"cwd":"/Users/ada","type":"system"}` captures a name running to
/// end-of-line, which then matches nothing.
///
/// # Errors
///
/// As `collect_owners`.
fn collect_owners_recursive(v: &Value, out: &mut Vec<String>) -> Result<(), String> {
    match v {
        Value::String(s) => collect_owners(s, out)?,
        Value::Array(a) => {
            for e in a {
                collect_owners_recursive(e, out)?;
            }
        }
        Value::Object(o) => {
            for (k, val) in o {
                collect_owners(k, out)?;
                collect_owners_recursive(val, out)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Redacts one captured wire trace. Whole-trace rather than per-line
/// because the identity also leaks as a bare name in prose, which is
/// only recognisable once a home path elsewhere in the file supplies it -
/// from the trace, so the same file redacts identically on any machine.
pub struct WireRedactor {
    owners: Vec<regex::Regex>,
}

impl WireRedactor {
    /// Discover the home-directory owner(s) named anywhere in `lines`. A
    /// trace naming no home path discovers none, so a bare name in its
    /// prose survives.
    ///
    /// # Errors
    ///
    /// A discovered name below `MIN_OWNER_LEN`.
    pub fn for_trace<'a>(lines: impl IntoIterator<Item = &'a str>) -> Result<Self, String> {
        let mut names: Vec<String> = Vec::new();
        for line in lines {
            match serde_json::from_str::<Value>(line) {
                Ok(v) => collect_owners_recursive(&v, &mut names)?,
                Err(_) => collect_owners(line, &mut names)?,
            }
        }
        // Longest first: the word-boundary anchors stop a short name
        // matching inside a longer one only while the extension is a word
        // character, and `first.last` home dirs are the case where it is
        // not. `sort_by` is stable, so ties keep discovery order.
        names.sort_by_key(|n| std::cmp::Reverse(n.len()));
        let owners = names
            .iter()
            .filter_map(|n| regex::Regex::new(&format!(r"\b{}\b", regex::escape(n))).ok())
            .collect();
        Ok(Self { owners })
    }

    /// Redact one line, structurally where a no-op round trip is
    /// byte-exact and by the text rules otherwise - serde_json parses
    /// some 17-digit floats to a neighbouring f64, and a non-JSON line
    /// has no structure to walk.
    ///
    /// # Errors
    ///
    /// Serialisation failure, and account or hook-body fields on a line
    /// that will not round-trip, which nothing but the structural rule
    /// reaches. That arm is LIVE in this build: a `Value` round trip is
    /// lossy on floats without `arbitrary_precision`, which would retire
    /// it. The message names the frame's `type`, never the body it
    /// refused to write.
    pub fn redact_line(&self, line: &str) -> Result<String, String> {
        let Ok(parsed) = serde_json::from_str::<Value>(line) else {
            return Ok(self.scrub_raw(line));
        };
        let reencoded = serde_json::to_string(&parsed).map_err(|e| format!("serialise: {e}"))?;
        if reencoded != line {
            let mut probe = parsed.clone();
            redact_account_recursive(&mut probe);
            redact_hook_body_recursive(&mut probe);
            redact_command_inventory_recursive(&mut probe);
            redact_mcp_config_recursive(&mut probe);
            if probe != parsed {
                let ty = parsed.get("type").and_then(Value::as_str).unwrap_or("<no type>");
                return Err(format!(
                    "a {ty} frame carries account, hook-body, command-inventory or \
                     mcp-config fields but does not survive a re-encode, so they cannot be redacted structurally"
                ));
            }
            return Ok(self.scrub_raw(line));
        }
        let mut v = parsed;
        redact_account_recursive(&mut v);
        redact_hook_body_recursive(&mut v);
        redact_command_inventory_recursive(&mut v);
        redact_mcp_config_recursive(&mut v);
        scrub_paths_recursive(&mut v);
        scrub_owner_tokens_recursive(&mut v, &self.owners);
        serde_json::to_string(&v).map_err(|e| format!("serialise: {e}"))
    }

    /// The text rules alone, for a line that must not be re-encoded.
    fn scrub_raw(&self, s: &str) -> String {
        replace_owners(&scrub_text(s), &self.owners)
    }
}

/// Recursively walk a JSON value and rewrite every absolute home path
/// segment to `<redacted-home>`. Defence-in-depth on top of the
/// structural redaction (which strips `text` / `tool_use_input` /
/// `tool_result_content`); catches any path leak that survives in
/// less-common fields.
fn scrub_paths_recursive(v: &mut Value) {
    match v {
        Value::String(s) if needs_scrub(s) => {
            *s = scrub_text(s);
        }
        Value::Array(a) => a.iter_mut().for_each(scrub_paths_recursive),
        Value::Object(o) => {
            // First scrub every value in place.
            for val in o.values_mut() {
                scrub_paths_recursive(val);
            }
            // Then scrub object KEYS too - fixture readers should see
            // no leaks even when a path appears as a map key (env-var
            // dumps, file-history snapshots, future Anthropic API
            // shapes that key by absolute path). Build a replacement
            // map only when at least one key changes.
            let needs_rewrite = o.keys().any(|k| needs_scrub(k));
            if needs_rewrite {
                let entries: Vec<(String, Value)> =
                    o.iter_mut().map(|(k, v)| (scrub_text(k), v.take())).collect();
                o.clear();
                for (k, v) in entries {
                    if o.contains_key(&k) {
                        // Two distinct source paths redacted to the
                        // same opaque key; the second insert silently
                        // overwrites. This is intentional (privacy
                        // collapse - the redactor should not preserve
                        // distinguishing path info), but log so a
                        // fixture-regen review can spot unexpected
                        // shape changes.
                        eprintln!(
                            "scrub_paths_recursive: key collision after redaction; \
                             distinct source paths collapsed into key={k:?}"
                        );
                    }
                    o.insert(k, v);
                }
            }
        }
        _ => {}
    }
}

/// Per-run state so that each distinct `session_id` / uuid gets a stable
/// opaque token. Reused across every line in the same transformation
/// pass so references inside a single session stay consistent.
#[derive(Default)]
pub struct RedactState {
    ids: HashMap<String, String>,
}

impl RedactState {
    /// Token-of-record for an arbitrary id. The first time we see a
    /// given input, we mint `<prefix>_<n>` (n monotonic per prefix).
    fn opaque(&mut self, prefix: &str, input: &str) -> String {
        if let Some(existing) = self.ids.get(input) {
            return existing.clone();
        }
        let n = self.ids.values().filter(|v| v.starts_with(&format!("{prefix}_"))).count();
        let out = format!("{prefix}_{n}");
        self.ids.insert(input.to_string(), out.clone());
        out
    }
}

/// Transform one persistence-format JSONL line into a stream-json
/// wire-shape line. Returns `Ok(None)` for entries that don't map
/// (persistence-only types like `file-history-snapshot`, `attachment`,
/// `last-prompt`).
///
/// # Errors
///
/// Returns a string describing the first shape mismatch. The caller
/// decides whether to propagate or skip the line.
pub fn transform_persistence_line(
    line: &str,
    state: &mut RedactState,
) -> Result<Option<String>, String> {
    let mut v: Value = serde_json::from_str(line).map_err(|e| format!("json parse: {e}"))?;
    let Some(ty) = v.get("type").and_then(Value::as_str).map(str::to_string) else {
        return Ok(None);
    };
    if !matches!(
        ty.as_str(),
        "assistant" | "user" | "system" | "result" | "rate_limit_event" | "stream_event" | "error"
    ) {
        return Ok(None);
    }

    let obj = v.as_object_mut().ok_or_else(|| "top-level not an object".to_string())?;

    // Rename sessionId → session_id.
    if let Some(sid) = obj.remove("sessionId")
        && let Some(s) = sid.as_str()
    {
        let opaque = state.opaque("session", s);
        obj.insert("session_id".into(), Value::String(opaque));
    }

    // Map parentUuid → parent_tool_use_id when present + non-null.
    if let Some(p) = obj.remove("parentUuid") {
        if let Some(s) = p.as_str() {
            let opaque = state.opaque("tool_use", s);
            obj.insert("parent_tool_use_id".into(), Value::String(opaque));
        } else {
            obj.insert("parent_tool_use_id".into(), Value::Null);
        }
    } else {
        obj.insert("parent_tool_use_id".into(), Value::Null);
    }

    // Opaque-map the top-level uuid.
    if let Some(u) = obj.get("uuid").and_then(Value::as_str).map(str::to_string) {
        let opaque = state.opaque("uuid", &u);
        obj.insert("uuid".into(), Value::String(opaque));
    }

    // Drop persistence-only fields the wire decoder doesn't expect.
    // `toolUseResult` / `lastPrompt` / `content` (snapshot body) carry
    // raw tool output / prompt text with embedded paths + project
    // content - must be removed entirely, not just scrubbed.
    for field in [
        "attachment",
        "content",
        "cwd",
        "entrypoint",
        "gitBranch",
        "isSidechain",
        "isMeta",
        "isSnapshotUpdate",
        "lastPrompt",
        "messageId",
        "operation",
        "permissionMode",
        "promptId",
        "requestId",
        "snapshot",
        "sourceToolAssistantUUID",
        "timestamp",
        "toolUseResult",
        "userType",
        "version",
    ] {
        obj.remove(field);
    }

    // Redact message content + tool input/result bodies.
    if let Some(msg) = obj.get_mut("message").and_then(Value::as_object_mut) {
        if let Some(Value::Array(content)) = msg.get_mut("content") {
            for block in content.iter_mut() {
                redact_content_block(block, state);
            }
        } else if let Some(Value::String(_)) = msg.get("content") {
            msg.insert("content".into(), Value::String("<redacted-text>".into()));
        }
        // Opaque-map the nested message id.
        if let Some(mid) = msg.get("id").and_then(Value::as_str).map(str::to_string) {
            msg.insert("id".into(), Value::String(state.opaque("msg", &mid)));
        }
    }

    // Defence-in-depth: scrub account fields and home paths from any
    // string anywhere in the transformed tree before serialising.
    // Catches leaks in fields the structural redactor doesn't enumerate
    // (Unknown blocks' arbitrary string fields, MCP tool names, server
    // error messages, etc.).
    redact_account_recursive(&mut v);
    scrub_paths_recursive(&mut v);

    let out = serde_json::to_string(&v).map_err(|e| format!("json serialise: {e}"))?;
    Ok(Some(out))
}

fn redact_content_block(block: &mut Value, state: &mut RedactState) {
    let Some(obj) = block.as_object_mut() else {
        return;
    };
    let ty = obj.get("type").and_then(Value::as_str).unwrap_or("").to_string();
    match ty.as_str() {
        "text" => {
            if let Some(Value::String(t)) = obj.get("text") {
                let bytes = t.len();
                obj.insert("text".into(), Value::String(format!("<redacted-text {bytes}b>")));
            }
        }
        "thinking" => {
            if let Some(Value::String(t)) = obj.get("thinking") {
                let bytes = t.len();
                obj.insert(
                    "thinking".into(),
                    Value::String(format!("<redacted-thinking {bytes}b>")),
                );
            }
            // `signature` is a signed opaque token - redact fully.
            if obj.contains_key("signature") {
                obj.insert("signature".into(), Value::String("<redacted-signature>".into()));
            }
        }
        "tool_use" => {
            // Keep `name` (informational), scrub `id` + `input`.
            if let Some(id) = obj.get("id").and_then(Value::as_str).map(str::to_string) {
                obj.insert("id".into(), Value::String(state.opaque("tool_use", &id)));
            }
            if obj.contains_key("input") {
                obj.insert("input".into(), serde_json::json!({"_redacted": true}));
            }
        }
        "tool_result" => {
            if let Some(id) = obj.get("tool_use_id").and_then(Value::as_str).map(str::to_string) {
                obj.insert("tool_use_id".into(), Value::String(state.opaque("tool_use", &id)));
            }
            if let Some(Value::String(c)) = obj.get("content") {
                let bytes = c.len();
                obj.insert(
                    "content".into(),
                    Value::String(format!("<redacted-tool-result {bytes}b>")),
                );
            } else if let Some(Value::Array(arr)) = obj.get_mut("content") {
                // Structured tool_result content - redact each sub-block.
                for sub in arr.iter_mut() {
                    redact_content_block(sub, state);
                }
            }
        }
        "document" | "image" => {
            // Anthropic API document/image block. Shape:
            // `{"type":"<kind>","source":{"type":"base64",
            //   "media_type":"<mime>","data":"<base64 bytes>"}}`.
            // The `data` field can be megabytes - replace with a
            // size-tagged stub (`<redacted-<kind>-data Nb>`) so the
            // fixture records the shape but not the content. Keep
            // `media_type` so fixtures document what kind of
            // attachment was present.
            if let Some(Value::Object(src)) = obj.get_mut("source") {
                if let Some(Value::String(d)) = src.get_mut("data") {
                    let bytes = d.len();
                    *d = format!("<redacted-{ty}-data {bytes}b>");
                }
                for k in ["text", "content", "url"] {
                    if let Some(Value::String(s)) = src.get_mut(k) {
                        let bytes = s.len();
                        *s = format!("<redacted-{ty}-{k} {bytes}b>");
                    }
                }
            }
        }
        _ => {
            // Unknown block type - keep shape, scrub any obvious
            // text-carrying fields to be safe.
            for (k, val) in obj.iter_mut() {
                if let Value::String(s) = val
                    && (k == "text" || k == "content" || k == "message")
                {
                    *val = Value::String(format!("<redacted-{} {}b>", k, s.len()));
                }
            }
        }
    }
}

/// Transform + redact an entire persistence .jsonl file into
/// stream-json-shaped output. One `TraceLog.entries` item per
/// decodable line, all tagged `"in"` (CLI → SDK direction).
///
/// # Errors
///
/// Returns the first line error if any non-persistence line refuses to
/// transform. Persistence-only lines are silently skipped.
pub fn redact_session_file(body: &str) -> Result<Vec<String>, String> {
    let mut state = RedactState::default();
    let mut out = Vec::new();
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match transform_persistence_line(line, &mut state) {
            Ok(Some(transformed)) => out.push(transformed),
            Ok(None) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(out)
}

/// Handy bundle: given a persistence file path, produce its redacted
/// stream-json lines plus a small summary string for logs.
///
/// # Errors
///
/// IO errors or per-line transform errors.
pub fn redact_session_path(path: &std::path::Path) -> Result<(Vec<String>, String), String> {
    let body = std::fs::read_to_string(path).map_err(|e| format!("read: {e}"))?;
    let lines = redact_session_file(&body)?;
    let summary = format!(
        "{}: in={} out={}",
        path.display(),
        body.lines().filter(|l| !l.trim().is_empty()).count(),
        lines.len()
    );
    Ok((lines, summary))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn scrub_paths_recursive_value_only_no_keys() {
        let mut v = json!({"k": "/Users/alice/foo.txt"});
        scrub_paths_recursive(&mut v);
        assert_eq!(v, json!({"k": "<redacted-home>/foo.txt"}));
    }

    #[test]
    fn scrub_paths_recursive_object_keys_rewritten() {
        let mut v = json!({"/Users/alice/foo.txt": 1, "harmless": 2});
        scrub_paths_recursive(&mut v);
        let obj = v.as_object().unwrap();
        assert!(obj.contains_key("<redacted-home>/foo.txt"));
        assert!(obj.contains_key("harmless"));
        assert_eq!(obj.len(), 2);
    }

    #[test]
    fn scrub_paths_recursive_collisions_collapse_last_wins() {
        // Two distinct source paths redact to the same opaque key.
        // Privacy-preserving by design - the redactor must not
        // expose distinguishing path info via key uniqueness.
        let mut v = json!({
            "/Users/alice/foo": "a",
            "/Users/bob/foo": "b",
        });
        scrub_paths_recursive(&mut v);
        let obj = v.as_object().unwrap();
        assert_eq!(obj.len(), 1);
        assert!(obj.contains_key("<redacted-home>/foo"));
    }

    #[test]
    fn scrub_paths_recursive_nested_object_keys_and_values() {
        let mut v = json!({
            "/Volumes/data/disk": {
                "inner_key_/home/x/foo": "/Users/x/y/z",
                "ok": 42
            }
        });
        scrub_paths_recursive(&mut v);
        // Outer key rewritten.
        let outer = v.as_object().unwrap();
        let inner = outer.get("<redacted-home>/disk").unwrap().as_object().unwrap();
        // Inner key rewritten too.
        assert!(inner.contains_key("inner_key_<redacted-home>/foo"));
        assert!(inner.contains_key("ok"));
        // Inner string value rewritten.
        assert_eq!(inner["inner_key_<redacted-home>/foo"], json!("<redacted-home>/y/z"));
    }

    #[test]
    fn scrub_paths_recursive_array_values() {
        let mut v = json!(["/Users/alice/x", "ok", {"k": "/home/bob"}]);
        scrub_paths_recursive(&mut v);
        let arr = v.as_array().unwrap();
        assert_eq!(arr[0], json!("<redacted-home>/x"));
        assert_eq!(arr[1], json!("ok"));
        assert_eq!(arr[2], json!({"k": "<redacted-home>"}));
    }

    /// Every key in the tree, at every depth.
    fn key_count(v: &Value) -> usize {
        match v {
            Value::Object(o) => o.len() + o.values().map(key_count).sum::<usize>(),
            Value::Array(a) => a.iter().map(key_count).sum(),
            _ => 0,
        }
    }

    /// One line standing in for a whole trace. Every caller's fixture
    /// round-trips, so this only exercises the structural path; the
    /// raw-text path is covered by the lossy-float test below. The key
    /// count guards against dropping a field, which no
    /// what-was-removed assertion can see.
    fn redact_one(line: &str) -> Value {
        let out =
            WireRedactor::for_trace([line]).expect("discovers").redact_line(line).expect("redacts");
        let after: Value = serde_json::from_str(&out).expect("redacted line stays valid JSON");
        let before: Value = serde_json::from_str(line).expect("fixture is valid JSON");
        assert_eq!(
            key_count(&before),
            key_count(&after),
            "redaction changed the key count\n  in:  {line}\n  out: {out}"
        );
        after
    }

    #[test]
    fn account_fields_are_placeholdered_and_keys_kept() {
        let v = redact_one(
            &json!({"response": {"response": {"account": {
                "email": "someone@example.com",
                "organization": "Some Org",
                "subscriptionType": "max",
                "apiProvider": "anthropic",
            }}}})
            .to_string(),
        );
        assert_eq!(
            v["response"]["response"]["account"],
            json!({
                "email": "<redacted-email>",
                "organization": "<redacted-organization>",
                "subscriptionType": "max",
                "apiProvider": "anthropic",
            })
        );
    }

    /// The shapes an `account` field turns up in besides a flat object.
    #[test]
    fn account_redaction_reaches_bare_nested_and_listed_forms() {
        let v = redact_one(
            &json!({
                "bare": {"account": "someone@example.com"},
                "nested": {"account": {"profile": {"email": "someone@example.com"}}},
                "listed": {"accounts": [{"email": "someone@example.com"}]},
            })
            .to_string(),
        );
        assert_eq!(v["bare"]["account"], json!("<redacted-account>"));
        assert_eq!(v["nested"]["account"]["profile"]["email"], json!("<redacted-email>"));
        assert_eq!(v["listed"]["accounts"][0]["email"], json!("<redacted-email>"));
    }

    #[test]
    fn profile_dir_is_redacted_home_relative_and_absolute() {
        let v = redact_one(
            &json!({"text": "see ~/.claude-alt/CLAUDE.md and /Users/alexandra/.claude-gateway1/x"})
                .to_string(),
        );
        assert_eq!(
            v["text"],
            json!("see ~/<redacted-profile>/CLAUDE.md and <redacted-home>/<redacted-profile>/x")
        );
    }

    #[test]
    fn project_slug_form_of_a_home_path_is_redacted() {
        let v = redact_one(
            &json!({"text": "projects/-Users-alexandra-Projects-forge/memory"}).to_string(),
        );
        assert_eq!(v["text"], json!("projects/<redacted-home>-Projects-forge/memory"));
    }

    /// The prose spellings a path regex cannot see, keyed off the home
    /// path on a different line of the same trace.
    #[test]
    fn owner_name_in_prose_is_redacted_from_a_home_path_elsewhere_in_the_trace() {
        let path_line = json!({"cwd": "/Users/alexandra/Projects/forge"}).to_string();
        let prose_line =
            json!({"text": "bundle id `dev.alexandra.app`, repo `alexandra/proxy`"}).to_string();
        let redactor =
            WireRedactor::for_trace([path_line.as_str(), prose_line.as_str()]).expect("discovers");
        let v: Value = serde_json::from_str(&redactor.redact_line(&prose_line).expect("redacts"))
            .expect("valid JSON");
        assert_eq!(
            v["text"],
            json!("bundle id `dev.<redacted-user>.app`, repo `<redacted-user>/proxy`")
        );
    }

    /// Over raw text this line captures a name running to end-of-line,
    /// which then matches nothing.
    #[test]
    fn owner_is_discovered_when_the_home_path_ends_the_json_string() {
        let line = json!({"cwd": "/Users/alexandra", "note": "ping alexandra"}).to_string();
        assert_eq!(redact_one(&line)["note"], json!("ping <redacted-user>"));
    }

    /// Stderr is what the non-JSON branch exists for, and it puts `:`
    /// straight after the path - a captured fragment matches nothing.
    #[test]
    fn an_owner_is_discovered_from_a_stderr_line_not_a_fragment_of_it() {
        let stderr = "EACCES: /Users/alexandra: permission denied";
        let prose = json!({"note": "owned by alexandra"}).to_string();
        let r = WireRedactor::for_trace([stderr, prose.as_str()]).expect("discovers");
        let v: Value = serde_json::from_str(&r.redact_line(&prose).expect("redacts")).unwrap();
        assert_eq!(v["note"], json!("owned by <redacted-user>"));
    }

    /// The slug has to be a discovery source, not only a redaction
    /// target.
    #[test]
    fn an_owner_is_discovered_from_the_slug_spelling_too() {
        let slug = json!({"p": "projects/-Users-alexandra-Projects-forge/x"}).to_string();
        let prose = json!({"note": "ping alexandra"}).to_string();
        let r = WireRedactor::for_trace([slug.as_str(), prose.as_str()]).expect("discovers");
        let v: Value = serde_json::from_str(&r.redact_line(&prose).expect("redacts")).unwrap();
        assert_eq!(v["note"], json!("ping <redacted-user>"));
    }

    /// Refused rather than skipped, reporting the length and never the
    /// name.
    #[test]
    fn an_owner_below_the_length_floor_is_refused() {
        let line = json!({"cwd": "/home/node"}).to_string();
        let err = WireRedactor::for_trace([line.as_str()]).err().expect("must refuse");
        assert!(err.contains("4-character"), "{err}");
        assert!(!err.contains("node"), "error names the owner: {err}");
    }

    /// Only a standalone occurrence is a name; inside a longer word it
    /// is a coincidence.
    #[test]
    fn an_owner_inside_a_longer_word_is_left_alone() {
        let line = json!({"cwd": "/Users/alexandra", "note": "alexandras alexandra"}).to_string();
        assert_eq!(redact_one(&line)["note"], json!("alexandras <redacted-user>"));
    }

    /// `first.last` home dirs: the word-boundary anchors do NOT cover
    /// this, since `\balexandra\b` matches inside `alexandra.bell`, so
    /// the longer name must be applied first.
    #[test]
    fn a_name_that_prefixes_another_does_not_strand_its_tail() {
        let short = json!({"cwd": "/Users/alexandra"}).to_string();
        let long = json!({"cwd": "/Users/alexandra.bell"}).to_string();
        let prose = json!({"note": "alexandra.bell"}).to_string();
        let r = WireRedactor::for_trace([short.as_str(), long.as_str(), prose.as_str()])
            .expect("discovers");
        let v: Value = serde_json::from_str(&r.redact_line(&prose).expect("redacts")).unwrap();
        assert_eq!(v["note"], json!("<redacted-user>"));
    }

    /// Claude Code's plugin-manifest dir is not a profile, and a fixture
    /// carrying a plugin path has no PII to lose.
    #[test]
    fn the_plugin_manifest_dir_is_not_taken_for_a_profile() {
        let line = json!({"p": "/tmp/x/.claude-plugin/marketplace.json"}).to_string();
        assert_eq!(redact_one(&line)["p"], json!("/tmp/x/.claude-plugin/marketplace.json"));
    }

    /// Keys go through the same rules as values, not the path rule alone.
    #[test]
    fn object_keys_get_every_rule_not_just_the_path_one() {
        let mut v = json!({"~/.claude-alt/settings.json": 1});
        scrub_paths_recursive(&mut v);
        assert!(v.as_object().unwrap().contains_key("~/<redacted-profile>/settings.json"));
    }

    /// The inbound tee records non-JSON stdout too, which no structural
    /// rule can reach.
    #[test]
    fn a_line_that_is_not_json_still_gets_the_text_rules() {
        let line = "node:internal/fs: ENOENT /Users/alexandra/.claude-alt/settings.json";
        assert_eq!(
            WireRedactor::for_trace([line]).expect("discovers").redact_line(line).as_deref(),
            Ok("node:internal/fs: ENOENT <redacted-home>/<redacted-profile>/settings.json")
        );
    }

    /// The raw-text path: the float survives while the path is still
    /// scrubbed. No `/…` tail on purpose - the path runs into the closing
    /// quote, where a class not excluding `"` eats the next field.
    #[test]
    fn a_redacted_line_that_cannot_round_trip_keeps_its_numbers_and_fields() {
        let line = r#"{"total_cost_usd":1.3382134999999997,"cwd":"/Users/alexandra","uuid":"a/b"}"#;
        let out =
            WireRedactor::for_trace([line]).expect("discovers").redact_line(line).expect("redacts");

        assert!(out.contains("1.3382134999999997"), "float was re-encoded: {out}");
        assert!(!out.contains("/Users/"), "path survived: {out}");
        let after: Value = serde_json::from_str(&out).expect("stays valid JSON");
        let before: Value = serde_json::from_str(line).expect("fixture is valid JSON");
        assert_eq!(key_count(&before), key_count(&after), "a field was dropped: {out}");
        assert_eq!(after["uuid"], json!("a/b"), "the field after the path was eaten: {out}");
    }

    /// Account fields are only reachable structurally, so a line that
    /// cannot be re-encoded must fail rather than half-redact.
    #[test]
    fn account_fields_on_a_non_round_tripping_line_fail_closed() {
        let line = r#"{"total_cost_usd":1.3382134999999997,"account":{"email":"a@b.co"}}"#;
        let err = WireRedactor::for_trace([line])
            .expect("discovers")
            .redact_line(line)
            .expect_err("must fail closed");
        assert!(err.contains("account"), "{err}");
        assert!(!err.contains("a@b.co"), "the error prints the value it refused to write: {err}");
    }

    /// A volume name is not a user name.
    #[test]
    fn volume_name_is_not_discovered_as_an_owner() {
        let line = json!({"a": "/Volumes/data/x", "b": "data set"}).to_string();
        assert_eq!(redact_one(&line)["b"], json!("data set"));
    }

    /// Re-encoding is lossy on some 17-digit floats, so a line with
    /// nothing to redact must come back untouched.
    #[test]
    fn a_line_with_nothing_to_redact_is_returned_verbatim() {
        let line = r#"{"type":"result","total_cost_usd":1.3382134999999997}"#;
        let r = WireRedactor::for_trace([line]).expect("discovers");
        assert_eq!(r.redact_line(line).as_deref(), Ok(line));
    }

    #[test]
    fn scrub_paths_recursive_no_op_on_clean_input() {
        let mut v = json!({"name": "alice", "n": 42, "list": [1, 2, "ok"]});
        let original = v.clone();
        scrub_paths_recursive(&mut v);
        assert_eq!(v, original);
    }
}
