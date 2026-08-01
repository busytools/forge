# Claude CLI upgrade check

This skill verifies forge stays compatible with the `claude` CLI after an upstream version bump. It catches three failure classes:

1. **Decoder drift** - the new CLI emits stream-json shapes (tool names, event subtypes, fields) that forge-sdk doesn't yet decode, producing `DecodedLine::Unknown` or decode errors. Sessions render incorrectly or break silently.
2. **Rewriter drift** - the new CLI's wire classification (headers, body keys, telemetry shape) shifted, so the existing forge-side rewriter no longer produces native-equivalent traffic. Hard Rule #16 violated.
3. **Tool surface drift** - the new CLI adds or removes tool primitives (e.g. `TodoWrite` → `TaskCreate/Update/List/Get` in 2.1.156) that forge-tui's inspector / chat renderers special-case. User-visible features silently regress.

The goal state: harness's `PINNED_CLI_VERSION` bumped to the new version, all sdk replay scenarios pass against fresh baselines, wire-equivalence-check still PASSes, and any drift is either decoded/rewritten or explicitly accepted with rationale.

## When to invoke

- After `brew upgrade claude-code` (or any other claude binary upgrade).
- When `claude --version` differs from `PINNED_CLI_VERSION` in `crates/forge-test-harness/src/sdk_wire.rs`.
- Before tagging a forge release - pairs with wire-equivalence-check as the two pre-release wire integrity checks.
- After a user reports forge behaves visibly differently than native on the wire OR a feature stopped rendering (renderer-side surface change).
- Periodically (monthly) as a drift check - Anthropic ships CLI updates frequently enough that 2-3 versions can stack up between explicit invocations.

## Pre-flight (agent runs)

Confirm the version delta and inspect the gap.

```bash
# 1. What's currently installed?
claude --version

# 2. What's the harness pinned to?
grep -n "PINNED_CLI_VERSION" crates/forge-test-harness/src/sdk_wire.rs

# 3. What baseline directories exist? (anything other than the pinned
#    version is leftover - note it for cleanup but don't delete yet.)
ls crates/forge-test-harness/baselines/sdk/

# 4. Confirm mitmproxy is available (the rewriter check + capture both
#    need it; install with `brew install mitmproxy` if missing).
which mitmdump || brew install mitmproxy
```

If installed == pinned: nothing to do. Skill exits clean.

If installed > pinned: continue with the workflow below. Report the delta so the user can see what they're about to verify.

## Phase 1: Tool surface diff (agent runs)

Probe the new CLI's init event for the representative models forge supports. Compare tool sets to spot adds/removes that forge-tui needs to handle.

```bash
mkdir -p /tmp/forge-cli-upgrade-check

# Probe each representative model. Capture init events to disk.
for MODEL in claude-opus-4-7 claude-opus-4-8 claude-haiku-4-5-20251001; do
  echo "hi" | claude --print --output-format stream-json --verbose \
    --model "$MODEL" 2>&1 \
    > "/tmp/forge-cli-upgrade-check/init-${MODEL}.jsonl" || true
done

# Extract + diff tool sets.
python3 << 'PYEOF'
import json, os, glob
results = {}
for path in sorted(glob.glob('/tmp/forge-cli-upgrade-check/init-*.jsonl')):
    label = os.path.basename(path).removeprefix('init-').removesuffix('.jsonl')
    with open(path) as f:
        for line in f:
            line = line.strip()
            if not line: continue
            try:
                d = json.loads(line)
            except Exception:
                continue
            if d.get('type') == 'system' and d.get('subtype') == 'init':
                results[label] = {
                    'cli_version': d.get('claude_code_version'),
                    'model': d.get('model'),
                    'tools': sorted(d.get('tools', [])),
                }
                break

# Pretty-print + union/intersection
all_tools = set()
for r in results.values():
    all_tools |= set(r['tools'])
common = set.intersection(*(set(r['tools']) for r in results.values()))
for label, r in results.items():
    only_here = set(r['tools']) - common
    print(f"{label}: cli={r['cli_version']} tools={len(r['tools'])} model_specific={sorted(only_here) or '(none)'}")
print(f"\nUnion across models: {sorted(all_tools)}")
print(f"Common across models: {sorted(common)}")
PYEOF
```

Compare the output against the previously-known tool set (search `crates/forge-tui/src/app/events/tool_calls.rs` for the gate string-match on tool names). Flag:

- **New tool names** forge-tui doesn't special-case yet. Inspector / chat suppression may need extension. File an issue per cluster.
- **Removed tool names** the renderer still special-cases. Dead code; mark for cleanup.
- **Same tool names but different `tool_input` shape** - only visible by capturing a real session and diffing. Phase 4 covers it.

### Scenario coverage audit (sub-step of Phase 1)

The baseline regeneration in Phase 4 only refreshes scenarios that already exist under `crates/forge-test-harness/tests/sdk_scenarios_*.rs`. New tools the CLI gained since the last upgrade have no scenario, so wire-shape changes for them go undetected.

```bash
# List current scenarios + the primary tool each exercises.
for f in crates/forge-test-harness/tests/sdk_scenarios_*.rs; do
  name=$(basename "$f" .rs | sed 's/sdk_scenarios_//')
  tools=$(grep -h "allowed_tools" "$f" 2>/dev/null | head -1 | sed 's/^[[:space:]]*//')
  echo "[$name] $tools"
done
```

Cross-reference against the Phase 1 tool union. For each tool in the union with NO matching scenario, decide:

- **High-signal wire surface** (new tool family with rich `tool_input` shape - e.g. TaskCreate/Update/List/Get, Workflow): file an issue for a follow-up scenario PR. Not a blocker for the upgrade - additive coverage.
- **Generic / low-shape tool** (e.g. ToolSearch which is a single-arg lookup): low priority; may not need a scenario.
- **Removed tool** (was covered, no longer in the union): retire the scenario file + delete its baseline.

The upgrade PR lands without these new scenarios; they ship as separate follow-up PRs over time. The skill flags the gap so the user / engineering team can prioritise.

## Phase 2: Decoder sanity against OLD baselines (agent runs)

Run the existing replay test suite while `PINNED_CLI_VERSION` still points at the OLD version. This confirms nothing regressed in the decoder against the previously-captured baselines before we touch anything.

```bash
cargo nextest run -p forge-test-harness --test sdk_replay 2>&1 | tail -30
```

If any scenario FAILs against its existing baseline, the decoder lost ground - investigate before regenerating. Otherwise proceed to bump + capture.

## Phase 3: Bump `PINNED_CLI_VERSION` (agent runs)

**Order matters here.** `baseline_dir()` resolves to `baselines/sdk/<PINNED_CLI_VERSION>/` via the compile-time constant in `crates/forge-test-harness/src/sdk_wire.rs`. If capture runs while PINNED is still the OLD version, captures land in the OLD directory and silently overwrite the existing baselines - destroying the comparison reference. Bump first, capture second.

The OLD baseline directory stays on disk (it's already committed). The bumped constant just makes the harness write to a fresh NEW dir.

```bash
OLD_VERSION=$(grep -oP 'PINNED_CLI_VERSION: &str = "\K[^"]+' crates/forge-test-harness/src/sdk_wire.rs)
NEW_VERSION=$(claude --version | awk '{print $1}')
echo "Bumping ${OLD_VERSION} -> ${NEW_VERSION}"

sed -i.bak "s/PINNED_CLI_VERSION: &str = \".*\"/PINNED_CLI_VERSION: &str = \"${NEW_VERSION}\"/" \
  crates/forge-test-harness/src/sdk_wire.rs
rm crates/forge-test-harness/src/sdk_wire.rs.bak

# Confirm the bump landed and OLD baselines are still on disk.
grep PINNED_CLI_VERSION crates/forge-test-harness/src/sdk_wire.rs
ls "crates/forge-test-harness/baselines/sdk/${OLD_VERSION}/" | wc -l
```

DO NOT run `cargo nextest run --test sdk_replay` here. It will panic - the harness expects `baselines/sdk/<NEW_VERSION>/<scenario>.jsonl` to exist, and we haven't captured them yet. The replay against new baselines comes in Phase 5.

## Phase 4: Baseline regeneration (agent runs)

Re-capture every scenario against the new CLI binary. With PINNED now pointing at NEW_VERSION, captures land in the fresh `baselines/sdk/<NEW_VERSION>/` directory; the OLD directory remains untouched for diffing.

```bash
# Capture all scenarios fresh. FORGE_WIRE_CAPTURE=1 tells the harness
# to write captures rather than replay-and-compare.
FORGE_WIRE_CAPTURE=1 cargo nextest run -p forge-test-harness \
  --no-capture --run-ignored only 2>&1 | tee /tmp/forge-cli-upgrade-check/capture.log

# Confirm the new dir exists with a baseline per scenario.
NEW_VERSION=$(grep -oP 'PINNED_CLI_VERSION: &str = "\K[^"]+' crates/forge-test-harness/src/sdk_wire.rs)
ls "crates/forge-test-harness/baselines/sdk/${NEW_VERSION}/" 2>&1 | wc -l
```

Diff old vs new baselines to surface wire changes:

```bash
OLD_VERSION=$(ls crates/forge-test-harness/baselines/sdk/ | grep -v "^${NEW_VERSION}$" | head -1)

for new in crates/forge-test-harness/baselines/sdk/${NEW_VERSION}/*.jsonl; do
  name=$(basename "$new")
  old="crates/forge-test-harness/baselines/sdk/${OLD_VERSION}/${name}"
  if [ ! -f "$old" ]; then
    echo "NEW scenario (no old baseline): $name"
    continue
  fi
  # JSON-aware diff: extract event types per line, compare type-sets.
  python3 -c "
import json, sys
def types(p):
    out = []
    with open(p) as f:
        for line in f:
            line=line.strip()
            if not line: continue
            try:
                d = json.loads(line)
                out.append((d.get('type'), d.get('subtype')))
            except Exception:
                out.append(('non-json', None))
    return out
old, new = types('$old'), types('$new')
old_set, new_set = set(old), set(new)
removed = old_set - new_set
added = new_set - old_set
if removed or added:
    print(f'=== ${name}: line_delta={len(new)-len(old):+d} removed_types={sorted(removed)} added_types={sorted(added)}')
"
done
```

For each scenario with deltas:

- **Added type/subtype** - new wire shape the decoder needs to handle. If the replay test passes against the new baseline (Phase 5 below), the decoder already handles it as a side-effect of the generic stream-json parser. If it fails, extend the decoder.
- **Removed type/subtype** - Anthropic dropped a shape. Check forge for code paths that depend on it; mark dead.
- **Same types but different counts** - usually a per-session-volume difference, not a wire change. Spot-check.

## Phase 5: Replay against NEW baselines (agent runs)

Now that fresh baselines exist under the bumped `PINNED_CLI_VERSION`, run the replay test to confirm the decoder parses every line cleanly.

```bash
cargo nextest run -p forge-test-harness --test sdk_replay 2>&1 | tail -30
```

If any scenario FAILs: the new baseline contains a stream-json shape the decoder doesn't handle. Extend `crates/forge-sdk/src/transport/codec.rs` to handle it, then re-run. Iterate until clean.

If all scenarios pass: the decoder is up to date with the new CLI's wire surface. Proceed.

## Phase 6: Wire-equivalence check (chain to existing skill)

The decoder-side is now confirmed. Verify the rewriter still produces native-equivalent traffic on the new CLI binary by chaining to the `wire-equivalence-check` skill:

```bash
# Invoke the existing skill (or run it manually). It captures forge +
# native side-by-side via mitmproxy and diffs every observable signal.
# See .claude/skills/wire-equivalence-check/SKILL.md for the full flow.
```

This is the same routine as a release pre-flight - the difference is that we run it NOW because the CLI changed. Expect WARN/INFO deltas that come from upstream additions; FAIL findings need rewriter fixes in `crates/forge-sdk/src/transport/proxy.rs` before the upgrade is acceptable.

## Phase 7: Renderer / inspector adjustments (if Phase 1 flagged adds)

If Phase 1 surfaced new tool names forge-tui doesn't handle (e.g. `TaskCreate` family), file a tracking issue per cluster and dispatch via the engineering team. The PR for those changes can land separately from the version bump PR if it's substantial, but the upgrade is incomplete until the renderer catches up. Pin the issue in the upgrade PR's description so future readers see the linkage.

## Phase 8: Land the upgrade

One PR with:

- `PINNED_CLI_VERSION` bumped in `crates/forge-test-harness/src/sdk_wire.rs`.
- New baselines under `crates/forge-test-harness/baselines/sdk/<NEW_VERSION>/`.
- Decoder patches (if any) in `crates/forge-sdk/`.
- Rewriter patches (if any) in `crates/forge-sdk/src/transport/proxy.rs`.
- Old baselines (`baselines/sdk/<OLD_VERSION>/`) deleted - they're frozen by `PINNED_CLI_VERSION` and the replay test only reads the pinned dir; keeping stale dirs is rot.

PR body should call out:
- The version delta (old → new).
- Any wire shape changes the diff surfaced.
- Any decoder/rewriter extensions made to handle them.
- Confirmation that wire-equivalence-check passed.
- Links to any renderer follow-up issues (Phase 6).

## Failure modes

**Tool surface diff is empty across all probed models** - the CLI is exposing the same tools at init. Either nothing changed, or models all happen to expose the same set (rare but possible). Sanity-check by inspecting one full init event dump (`cat /tmp/forge-cli-upgrade-check/init-claude-opus-4-7.jsonl | head -1 | python3 -m json.tool`).

**Capture mode produces empty `.jsonl` files** - the harness's capture-env didn't trigger. Confirm `FORGE_WIRE_CAPTURE=1` is exported, and that the scenarios are actually `#[ignore]`-gated (only `--run-ignored only` runs them). Check `crates/forge-test-harness/tests/sdk_scenarios_*.rs` for the gate.

**Replay test passes against OLD baselines but FAILs against NEW** - most common failure. The new CLI emits a shape the decoder doesn't recognize. Look at the failing scenario's NEW baseline, find the unfamiliar `type/subtype`, extend the decoder.

**`claude --version` matches `PINNED_CLI_VERSION` but rewriter check FAILs** - likely a forge-sdk regression unrelated to the upgrade. Run wire-equivalence-check directly to isolate.

**Baseline diff shows many scenarios with "removed types"** - Anthropic dropped a wire shape. Search forge source for code paths that depend on it; some may need to be removed or rewritten. Don't just delete the baseline coverage - the decoder still needs to handle the old shape gracefully if any long-lived process on the old CLI might still emit it (e.g. a running forge subprocess that hasn't restarted yet).

## Quick start (one-liner orientation)

```
claude --version                         # what's installed now
grep PINNED_CLI_VERSION crates/forge-test-harness/src/sdk_wire.rs   # what's pinned
ls crates/forge-test-harness/baselines/sdk/                          # what baselines exist
```

If installed > pinned: run the workflow above. If equal: nothing to do.

## Reference materials

- Pinned version constant: `crates/forge-test-harness/src/sdk_wire.rs` (search for `PINNED_CLI_VERSION`).
- Baselines: `crates/forge-test-harness/baselines/sdk/<version>/*.jsonl`.
- Replay test: `crates/forge-test-harness/tests/sdk_replay.rs`.
- Capture scenarios: `crates/forge-test-harness/tests/sdk_scenarios_*.rs`.
- Tool-name gate (forge-tui): `crates/forge-tui/src/app/events/tool_calls.rs` (search for the string-match against tool names).
- Decoder: `crates/forge-sdk/src/transport/codec.rs`.
- Rewriter: `crates/forge-sdk/src/transport/proxy.rs`.
- Hard Rule #16 (wire classification must match native CLI): top-level `CLAUDE.md`.
- Paired skill: `.claude/skills/wire-equivalence-check/SKILL.md`.

## Wire-conformance cheatsheet (outside an upgrade)

Replay mode runs on every `cargo nextest run`: offline, no API cost. The
harness lives in `crates/forge-test-harness/`.

Live-capture a single scenario:

```bash
FORGE_WIRE_CAPTURE=1 cargo nextest run -p forge-test-harness \
    --no-capture --run-ignored only sdk_<test>
```

Adding a scenario (Hard Rule #9 requires one for any new wire surface):
write `tests/sdk_scenarios_<name>.rs`, run it with the env var above,
copy the capture into
`baselines/sdk/<PINNED_CLI_VERSION>/`, and commit the test plus its
baseline together. Some fixtures are NOT capture-produced: the
`real_session_*` multimodal ones come from redacted on-disk sessions via
the `sdk_redact_session` example, so deleting a baseline directory
silently drops image and document coverage.
