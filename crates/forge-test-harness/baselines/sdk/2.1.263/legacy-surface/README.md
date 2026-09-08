# legacy-surface corpus

The same capture scenarios as the parent directory, recorded with the
CLI's ambient routing on a model id it does not recognize (a
proxy-backed model). The CLI tailors its tool surface by model
recognition: recognized Anthropic models get a pruned built-in set
(26 tools at 2.1.263), while unrecognized ids keep the legacy full
surface - the Task* family is present here and absent from the parent
corpus.

The tailoring keys on the auth route, not only the model id: the
`set_model` scenario pins a recognized model id (`claude-sonnet-4-6`)
yet still gets the legacy 29-tool surface under the proxy route,
where the same id under direct Anthropic auth gets the pruned one.
`Monitor` is absent on BOTH surfaces at 2.1.263; the Task* family is
the legacy-surface-only part.

Both surfaces are forge's running reality: forge.toml accounts route
through Anthropic tokens and through proxies alike, so the decoder
must round-trip what the CLI emits on either. Replay covers this
directory via `all_legacy_baselines_decode_cleanly`; the capture
hygiene gate redaction-checks it like the parent corpus.

Captured with the same `PINNED_CLI_VERSION` binary as the parent
corpus. The capture env differs only in the ambient model routing -
see `.claude/skills/claude-cli-upgrade/` for the dual-corpus ritual.
The parent-corpus-only baselines (`real_session_*` from the redaction
pipeline, `stop_hook_error` from the notification scenario) are
deliberately absent here.
