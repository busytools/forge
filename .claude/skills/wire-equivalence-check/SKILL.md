---
name: wire-equivalence-check
description: Verify forge is wire-indistinguishable from native `claude` CLI on every observable signal. Drives a paired mitmproxy capture (forge + native, side-by-side, same project) then runs an exhaustive diff across every observable dimension — six known classification channels, full header sets per endpoint, query params per endpoint, JSON-key paths in request bodies, response status codes, telemetry event-name distributions, anthropic-beta header values, body fingerprints, and a defensive pattern scan for any sdk-* / agent-sdk leak anywhere in any body. Designed for iterative tightening: run → fix the highest-priority FAIL → re-run, until zero FAIL findings. Writes timestamped audit JSON per run so consecutive invocations show "fixed since last run" vs "newly introduced". Invoke when claude CLI updates, when forge's transport/proxy code changes, before a forge release, or any time wire equivalence might have regressed.
---

# Wire-equivalence check

This skill verifies forge is wire-indistinguishable from native `claude` CLI on every observable network signal — request URL, headers, query parameters, body content, telemetry payloads — except for explicitly-accepted divergences. It runs an exhaustive comparison and surfaces every difference, ranked by severity.

The goal state: zero `FAIL` findings, zero unaccepted `WARN` findings. Iterate until reached.

## When to invoke

- Native `claude` released a new version (`claude --version` differs from last known). New versions may add endpoints, change header shapes, or rename classification fields. Forge's rewriter may need extending.
- Forge's `crates/forge-sdk/src/transport/proxy.rs` or `crates/forge-agent/` HTTP client code changes — verify no regression.
- Before tagging a forge release.
- A user reports forge behaves visibly differently from native on the wire.
- Quarterly or post-incident integrity check.

## What "wire-equivalent" means here

Forge is wire-equivalent to native if, with the same project and equivalent user workflow:

1. **All six classification channels match exactly** between native and forge (entrypoint=cli, is_interactive=true, etc.)
2. **Forge's endpoint set is a subset of native's**, except for the accepted-divergences listed in `accepted-divergences.json` (currently: `/api/oauth/usage`, `registry.npmjs.org/@anthropic-ai/claude-code/latest`, `status.claude.com/api/v2/summary.json` — all explicitly accepted by the forge team)
3. **No `sdk-*` string appears anywhere** in any request body, header, ddtag, or URL — at any nesting level, including stringified JSON
4. **`agent_sdk_version` field is absent** in every telemetry event on both sides
5. **Headers, query parameters, and request-body JSON-key paths match** between same-endpoint requests on both sides (modulo expected per-request volatility like timestamps and request IDs)
6. **Response status code distributions match** per endpoint
7. **Telemetry event_name distributions match** between sides

The analyzer surfaces every observable difference. Items in classes 1-4 are `FAIL` (block release). Items in 5-7 are `WARN` (investigate, may or may not need a fix — sometimes they're legitimate cross-session variation).

## Iterative tightening workflow

This is not a one-shot test. The skill is designed for repeated runs as forge fixes findings:

```
[run]  → capture native + forge in two panes → analyze → see N FAIL + M WARN
   ↓
[fix]  → forge team patches highest-priority FAIL
   ↓
[run]  → capture again → analyze → see "fixed since last run: +0/-N1; new: +0/-0"
   ↓
[iterate until]  zero FAIL, zero unaccepted WARN
```

The analyzer writes a timestamped audit JSON per run to `~/Projects/forge/audits/wire-equivalence/audit-YYYY-MM-DD-HHMMSS.json`. Consecutive runs auto-compare and report delta (e.g., "FAIL -1, WARN -3"). The cycle is meant to converge.

## Step-by-step workflow

The agent drives every step except the user-running-the-binaries part. The agent runs each command shown below, surfaces results to the user, and produces the verdict at the end.

### Step 1 — pre-flight (agent runs)

```bash
# 1. Ensure mitmproxy is installed
which mitmdump || brew install mitmproxy

# 2. Ensure mitmproxy CA exists (auto-generates on first run)
[ -f ~/.mitmproxy/mitmproxy-ca-cert.pem ] || {
    mitmdump --listen-port 19999 --quiet > /dev/null 2>&1 &
    sleep 2
    pkill -f 'mitmdump.*--listen-port 19999' 2>/dev/null
}

# 3. Confirm both binaries are reachable + record versions
claude --version  # record for audit
forge --version   # record for audit

# 4. Install mitmproxy Python lib if needed (analyzer uses it)
python3 -c 'from mitmproxy import io' 2>/dev/null || pip3 install --break-system-packages --quiet mitmproxy
```

### Step 2 — boot both mitmproxies (agent runs)

```bash
bash ~/Projects/forge/.claude/skills/wire-equivalence-check/scripts/setup-proxies.sh
```

The helper boots two mitmproxies, native on 9001 and forge on 9002, both writing to `/tmp/forge-wire-check/`. Logs go to the same directory.

### Step 3 — give the user the two commands to run

The agent presents these two commands as copy-pasteable blocks. **Both must run against the same project** so the only variable is the binary. The session content doesn't matter, but the volume of activity should be roughly comparable on both sides (a couple of chat prompts plus at least one tool call).

**Pane 1 — native:**

```zsh
unset CLAUDECODE CLAUDE_CODE_ENTRYPOINT
export HTTPS_PROXY=http://127.0.0.1:9001 \
       HTTP_PROXY=http://127.0.0.1:9001 \
       NODE_EXTRA_CA_CERTS="$HOME/.mitmproxy/mitmproxy-ca-cert.pem" \
       SSL_CERT_FILE="$HOME/.mitmproxy/mitmproxy-ca-cert.pem" \
       CURL_CA_BUNDLE="$HOME/.mitmproxy/mitmproxy-ca-cert.pem" \
       REQUESTS_CA_BUNDLE="$HOME/.mitmproxy/mitmproxy-ca-cert.pem"
cd <SAME-PROJECT-ROOT>
claude --model claude-haiku-4-5-20251001
```

**Pane 2 — forge:**

```zsh
unset CLAUDECODE CLAUDE_CODE_ENTRYPOINT
export HTTPS_PROXY=http://127.0.0.1:9002 \
       HTTP_PROXY=http://127.0.0.1:9002 \
       NODE_EXTRA_CA_CERTS="$HOME/.mitmproxy/mitmproxy-ca-cert.pem" \
       SSL_CERT_FILE="$HOME/.mitmproxy/mitmproxy-ca-cert.pem" \
       CURL_CA_BUNDLE="$HOME/.mitmproxy/mitmproxy-ca-cert.pem" \
       REQUESTS_CA_BUNDLE="$HOME/.mitmproxy/mitmproxy-ca-cert.pem"
cd <SAME-PROJECT-ROOT>
forge --model claude-haiku-4-5-20251001
```

**User instructions (agent says this verbatim):**

> Two terminal panes. Pane 1 runs native, Pane 2 runs forge. Same project in both. Do whatever workflow you want — a couple of chat prompts + at least one tool call. Doesn't matter what you chat about; what matters is that both panes do similar amounts of work. Exit cleanly with `/exit` in both panes. Tell me "done both" when finished.

### Step 4 — wait for "done both", then stop proxies (agent runs)

```bash
pkill -INT -f 'mitmdump.*--listen-port 900' 2>/dev/null
sleep 2
ls -la /tmp/forge-wire-check/flows-*.mitm
```

Both files should be present and >100KB. If either is 0 bytes, jump to "Failure modes" below.

### Step 5 — run the exhaustive analyzer (agent runs)

```bash
python3 ~/Projects/forge/.claude/skills/wire-equivalence-check/scripts/analyze.py \
    --native /tmp/forge-wire-check/flows-native.mitm \
    --alt    /tmp/forge-wire-check/flows-alt.mitm \
    --accepted ~/Projects/forge/.claude/skills/wire-equivalence-check/accepted-divergences.json \
    --verbose
```

The analyzer outputs findings grouped by severity (`FAIL` → `WARN` → `INFO` → `ACCEPTED`). It also writes an audit JSON and prints the delta vs. the previous run.

### Step 6 — interpret findings + report verdict

The agent reads the analyzer output and:

- **If zero `FAIL`:** report PASS with binary versions tested and `WARN`/`INFO`/`ACCEPTED` counts. Done.
- **If any `FAIL`:** report FAIL with the specific findings, prioritized by category. For each, suggest the likely fix location in forge source.

Use this priority order for `FAIL` items:

1. **Classification regressions** (`sdk-*` string anywhere, `agent_sdk_version` reappearance, system prompt `cc_entrypoint` not cli) — these directly affect what Anthropic sees and bills on. Fix immediately.
2. **B1/B3 endpoint regressions** (`count_tokens`, `metrics_enabled` coming back) — a previously-fixed call has re-introduced itself. Check recent forge commits.
3. **Unaccepted endpoint divergence** — forge added a new endpoint native doesn't have. Either route via embedded rewriter, drop the call, or update `accepted-divergences.json` with rationale.
4. **Telemetry classification stray values** — a previously-handled field has a new value or moved to a new path.
5. **MCP UA leak** — third-party MCP server is receiving an SDK-shape UA.

For each, point at the fix location: `crates/forge-sdk/src/transport/proxy.rs` for body / header rewriting; `crates/forge-agent/src/http_trust.rs` if a new reqwest client needs TLS cert loading; `crates/forge-agent/{cloud,env}/*.rs` if a new outbound endpoint was added.

Reference fix patterns are in `~/.claude/memory/brief_claude_cli_rewriter_implementation_2026_05_20.md` (architecture context) and `~/.claude/memory/reference_claude_cli_integration_modes.md` (empirical evidence and the 6 channels).

## What `WARN` items mean (not always failures)

The exhaustive analyzer surfaces many cross-binary differences that aren't necessarily problems:

- **Different headers in same endpoint**: native might send a slightly different `anthropic-beta` value because forge was built against a different CLI version. Triage: is the difference meaningful for classification?
- **Different JSON paths in request bodies**: e.g., native sends `metadata.user_id` and forge sends `metadata.user_id` plus `metadata.user_uuid`. Triage: is the extra field a forge-specific add (potential leak) or a legitimate new field (e.g., new claude version)?
- **Different telemetry event names**: forge might emit additional event types specific to its functionality.
- **Different response status distributions**: forge or native might have one endpoint returning 401 vs 200 depending on auth state.

For WARN items, the workflow is:

1. Read the analyzer output's `--verbose` detail for the specific difference
2. Triage: is this a classification leak in disguise (e.g., a new key carrying `sdk-cli`), an accepted divergence (forge feature), or noise (per-session variation)?
3. Either fix forge, update `accepted-divergences.json`, or move on if it's noise

The iterative cycle naturally reduces WARN count over time.

## Failure modes during capture

If something goes wrong during capture (not analysis):

**Both flow files are 0 bytes:**
- User didn't actually run the binaries.
- `HTTPS_PROXY` env var wasn't exported — check user's shell history.
- Wrong mitmproxy port in commands.

**Only one flow file is 0 bytes:**
- That binary didn't run, OR can't trust the mitmproxy CA. Check `/tmp/forge-wire-check/mitm-<side>.log` for `Client TLS handshake failed` lines.
- For port 9001 (native): native respects `NODE_EXTRA_CA_CERTS` — check user actually exported it.
- For port 9002 (forge): if forge fails handshake, it's a regression on env-var honoring. Check `crates/forge-sdk/src/transport/proxy.rs::build_outbound_tls_config` and `crates/forge-agent/src/http_trust.rs` are intact (commits `d3f33f1` + `d5f9f2a` introduced the original handling).

**Capture has traffic but no `/v1/messages` calls:**
- User exited before any model interaction. Ask them to do at least one prompt.

**Capture has classification signals but very few telemetry events:**
- User killed the session too quickly (Ctrl-C instead of `/exit`). Telemetry batches flush periodically and at clean exit. Ask for a clean `/exit`.

**Suspect "the binary doesn't have the fix" based on `strings | grep`:**
- Symbol-hunting via `strings` can mislead. LLVM's link-time string-fragment optimization may eliminate constant string literals from the final binary even when the function that references them is alive. The forge binary in Round 5/6 showed zero hits for expected literal strings (`claude-code-20250219`, `effort-2025-11-24`) via `strings` even though the `rewrite_anthropic_beta` function symbol was present in `nm` output and the log messages from that function were in the binary. The function WAS in fact live at runtime.
- **Diagnostic protocol when investigating "is the fix in the binary?":**
  1. `nm ~/.cargo/bin/forge | grep <function_name>` — if the function symbol is present, the fix is in. Trust this signal.
  2. `strings ~/.cargo/bin/forge | grep <log_message_literal>` — if the diagnostic log message text from the fix function is in the binary, the function is also live.
  3. If both above are positive but the LITERAL CONSTANT strings (e.g., the strings the function uses as its compare-against list) are missing, that's LLVM string-fragment optimization. The runtime behavior is unaffected. Stop diagnosing and run the actual capture.
  4. Only if `nm` shows no function symbol at all AND log messages are missing is the fix genuinely not in the binary. In that case, rebuild.

**Reinstall protocol after a forge-side fix:**
- `cargo install --path crates/forge-tui --force` is required after every forge fix. `cargo build` updates rlibs in `target/release/` but does NOT re-link the binary at `~/.cargo/bin/forge`. Missing this step caused Round 5 to test a stale binary.
- After install, verify the binary mtime is later than the commit time of the latest fix. Confirms ordering but not content (see symbol-hunting note above).
- When forge-sdk changes specifically: both `cargo build --release -p forge-sdk` (rebuild rlib) AND `cargo install --path crates/forge-tui --force` (re-link binary) are needed in that order.

**Subprocess tool calls (terraform, gh, git, curl, etc.) fail with TLS errors during the session:**
- Expected and harmless. Go binaries on macOS use SecureTransport via cgo, which ignores `SSL_CERT_FILE` — they only trust the macOS keychain. Doesn't affect the wire-equivalence test because we measure claude/forge's own classification signals, not subprocess success. If a user wants full subprocess proxy coverage (e.g., to actually use terraform during a captured session), they need to install the mitmproxy CA into the macOS keychain once:
  ```bash
  sudo security add-trusted-cert -d -r trustRoot \
    -k /Library/Keychains/System.keychain \
    ~/.mitmproxy/mitmproxy-ca-cert.pem
  ```
  But this is system-wide trust — overkill for one capture. Easier path: have the user skip subprocess-heavy tool calls during the test session (use Read/Edit/Bash on local commands instead of terraform/gh/curl).

## Accepted divergences

`accepted-divergences.json` in this directory lists endpoints forge intentionally hits that native doesn't. The analyzer reads it and downgrades those from FAIL to ACCEPTED. To add or remove entries, edit the JSON directly.

Each entry should have a `reason` explaining why the divergence is acceptable, who accepted it (forge-team or wire-equivalence-skill), and a `review_after` date to revisit. Default review cadence: quarterly.

## Audit trail

Each invocation writes `~/Projects/forge/audits/wire-equivalence/audit-<timestamp>.json`. The analyzer reads the most recent prior audit and prints a delta line (FAIL +/-N, WARN +/-M) so you can see progress across runs. Keep these audits committed to git for long-term tracking of forge's wire-equivalence drift over time.

## Reference materials (full background context)

- **Empirical evidence** for the 6 classification channels + capture methodology: `~/.claude/memory/reference_claude_cli_integration_modes.md`
- **Original implementation brief** for the forge wire-rewriter: `~/.claude/memory/brief_claude_cli_rewriter_implementation_2026_05_20.md`
- **Past round results** (what each fix-round addressed): `/tmp/forge-wire-equivalence-handoff-2026-05-20.md` and `/tmp/forge-round2-results-2026-05-20.md`, if still present in `/tmp`
- **The forge proxy implementation itself**: `crates/forge-sdk/src/transport/proxy.rs`, `crates/forge-agent/src/http_trust.rs`
- **The CLI binary classification logic** (for understanding why these signals exist): the `H9q()` function in the claude binary, see `~/.claude/memory/reference_claude_cli_integration_modes.md` § "binary reverse-engineering"
