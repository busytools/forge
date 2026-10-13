# legacy-surface corpus

The parent directory's capture scenarios as they stood at this corpus's
capture, recorded with the CLI's ambient routing on a model id it does not
recognize. The CLI tailors its tool surface per model, so this corpus
records the surface a proxy-backed forge account actually gets. Three
baselines stayed parent-only and are named at the end.

At 2.1.280 that surface is 20 built-in tools on
`deepseek-v4.1-flash[1m]`, 23 in the scenarios that also expose the
interactive group (`AskUserQuestion`, `EnterPlanMode`, `ExitPlanMode`).
The Task family in it is `Task` and `TaskStop` alone. It is a subset of
the recognized surface rather than a superset: `RemoteTrigger` and
`ToolSearch` are absent here, and the `TaskCreate` / `TaskGet` /
`TaskList` / `TaskUpdate` quartet appears in the parent corpus instead,
on `claude-sonnet-4-6`. The older "unrecognized ids keep the full legacy
surface" story does not hold.

Two things make a cross-version count comparison misleading, so read the
Task-family delta rather than the totals. The ambient model differs
between the corpora (`z-ai/glm-5.3-flash[1m]` at 2.1.263,
`deepseek-v4.1-flash[1m]` at 2.1.280), and 2.1.280 was captured against a
scratch `CLAUDE_CONFIG_DIR`, so config-driven tools
(`ListMcpResourcesTool`, `ReadMcpResourceDirTool`, `ReadMcpResourceTool`,
`LSP`) are absent from it by construction and not because the CLI
dropped them. On the models that appear in both corpora
(`claude-opus-5[1m]` and `claude-sonnet-4-6`) the Task family is
unchanged apart from `TaskOutput`, which is gone from every frame.

Both surfaces are forge's running reality: forge.toml accounts route
through Anthropic tokens and through proxies alike, so the decoder must
round-trip what the CLI emits on either. Replay covers this directory
via `all_legacy_baselines_decode_cleanly`, and the capture hygiene gate
redaction-checks it like the parent corpus.

Captured with the same `PINNED_CLI_VERSION` binary as the parent corpus.
See `.claude/skills/claude-cli-upgrade/` for the dual-corpus ritual.

Three baselines are parent-only. The `real_session_*` triple comes from
the redaction pipeline rather than a capture, and `set_model` drives
`claude-sonnet-4-6`, which the capture machine's gateway org does not
serve, so the scenario 503s here; the same control flow is covered by the
parent corpus. `queued_prompt_lifecycle` postdates this corpus - it landed
2026-10-03 with the prompt-id stamp, after this directory was captured
2026-09-23 - and has not been recaptured: what it records is the prompt
queue's own lifecycle rather than a tool-surface shape, so the
model-tailored surface this directory exists to hold is not what it
measures.
