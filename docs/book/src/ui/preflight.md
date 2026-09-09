# Preflight - the launchpad's first view

The launchpad is two views, and **preflight** is the first thing forge renders **on every route**. It resolves the accounts and, when dictation is switched on, fetches, verifies and loads its models, then hands over to wherever the invocation was headed: the projects view for `forge`, straight into chat for `forge <project>`. Shown once per run; every later `/launchpad` goes straight to the projects view.

**Nothing spawns until every account has settled.** The assignment plan is only computed once they have, so a session started before that falls back to round-robin and can land on an account the project's org does not allow. Accounts rather than the whole of preflight, because the plan needs them and does not need the dictation weights - so the models keep loading alongside the session rather than delaying it.

**Preflight completes when every account settles, not only when every account is `Ready`.** A bailed account rides along as degraded rather than holding boot: the assignment plan excludes it, the pollers keep re-probing it, and its row names the failure. Every failure state names its exits rather than leaving the reader on a screen with nothing to press. **Repairing an account's auth means editing `forge.toml`, and that needs a restart.** The env is read once at boot, so the pollers keep probing what they loaded until forge restarts. The screen states the restart without a number, because the pollers run under probe backoff and no single interval would be true.

## Preflight, resolving

*visible: `ActiveView::Launchpad` before the hand-over*

Two sibling sections at the same indent, sharing one row shape: two-cell indent, state glyph, name, right-aligned twelve-cell state column. The panel is `PICKER_WIDTH` - the same width the projects view uses - so the hand-over is a content swap rather than a resize. Model rows read by **role** with the file on a dim continuation line beneath: `transcribing model (cohere-transcribe-03-2026-Q4_K_M)` is 53 cells against a 38-cell name column. There is no summary line.

<div class="term">

  <pre class="indent">
            <span class="rust-orange bold">███████╗ ██████╗ ██████╗  ██████╗ ███████╗</span>
            <span class="rust-orange bold">██╔════╝██╔═══██╗██╔══██╗██╔════╝ ██╔════╝</span>
            <span class="rust-orange bold">█████╗  ██║   ██║██████╔╝██║  ███╗█████╗  </span>
            <span class="rust-orange bold">██╔══╝  ██║   ██║██╔══██╗██║   ██║██╔══╝  </span>
            <span class="rust-orange bold">██║     ╚██████╔╝██║  ██║╚██████╔╝███████╗</span>
            <span class="rust-orange bold">╚═╝      ╚═════╝ ╚═╝  ╚═╝ ╚═════╝ ╚══════╝</span>
                          <span class="dim">v1.0.53+3cda0dee</span>
                          <span class="dim">claude 2.1.263</span>

      <span class="dim">────────────────────────────────────────────────────────</span>
        <span class="dim bold">Accounts</span>
        <span style="color:#7eb87a">●</span> Subspace                              <span class="dim">       ready</span>
        <span style="color:#7eb87a">●</span> Granite                               <span class="dim">       ready</span>
        <span style="color:#cfc26b">○</span> Granite1                              <span class="dim">   resolving</span>
        <span style="color:#cfc26b">○</span> Personal                              <span class="dim">   resolving</span>
        <span style="color:#cfc26b">○</span> Codex                                 <span class="dim">   resolving</span>

        <span class="dim bold">Dictation</span>
        <span class="rust-orange">⠹</span> transcribing model                    <span class="dim">   verifying</span>
          <span class="dim">(cohere-transcribe-03-2026-Q4_K_M)</span>
        <span class="rust-orange">⠹</span> normalization model                   <span class="dim">   verifying</span>
          <span class="dim">(s1-mini-f16)</span>
      <span class="dim">────────────────────────────────────────────────────────</span>

 <span class="dim">ctrl+q  quit</span></pre>

</div>

- **code** - `crates/forge-tui/src/ui/preflight.rs` (renderer) + `crates/forge-tui/src/app/preflight.rs` (hand-over latch + keyboard handler) · progress state from `crates/forge-workspace/src/dictate.rs`
- **spinner** - The configured `SpinnerStyle` at its own cadence, through the same `App::active_spinner_glyph` every other animated surface reads. Change `[ui] spinner` and this moves with the rest.
- **account states** - `○` yellow `resolving` = Loading · `●` green `ready` = probe returned · `⚠` red = Bailed on an auth failure, one label per class: `auth failed` = rejected credentials · `⚠` yellow = Bailed on a transient failure, one label per class: `unreachable` = the probe never got through, `fetch error` = a classed-but-unrecognised failure (a 5xx proxy, a body that will not decode, or a 200 whose body maps to nothing), `rate limited` = a 429 that waiting clears. The glyph colour splits by class the same way on the launchpad's chip row, which shares `account_glyph`.
- **model states** - `queued` · `downloading` · `resuming` (a transfer that picked up a `.part`) · `verifying` · `ready` · `loading` · then `ready` again once the weights are in memory. A failure reads `bad hash` or `cancelled`, and a row nothing will now start reads `not started` rather than `queued`.
- **both rows advance together** - `forge_dictate::prepare` gives each model a thread, so the pair costs the slower of the two rather than their sum - 5.3 s to 2.7 s verifying the shipped 3.07 GB warm. `queued` is therefore the moment before both start, not one model waiting on the other.
- **keys** - <kbd>Esc</kbd> cancels an in-flight model download, which quits forge · <kbd>Ctrl+Q</kbd> quit. Every other key is consumed silently - there is nothing to navigate and the projects view underneath is not reachable yet.
- **hand-over** - Latched, not re-evaluated. A token expiring mid-session takes an account `Ready → Bailed → Loading`, so the readiness condition genuinely goes false again while the user is working; without the latch that would throw them back onto a boot screen. The launchpad's own gate covers that window instead.
- **short terminals** - The wordmark is dropped before any panel content when the block does not fit, because the failure screens' exits are the one thing this screen cannot clip. Past that the panel drops rows from the TOP rather than the bottom - the failure detail and the exits it names are appended last - and replaces them with a dim `… N more above`, so nothing vanishes unmarked. At 100x24 the bailed screen already overflows.
- **no verify bar** - Verifying draws a spinner and no byte counter. `forge_dictate::Progress::Verifying` carries only a file name - hashing checkpoints cancellation rather than reporting progress.

## Preflight fails: a bailed account

*rides along as degraded - preflight hands over once the rest settles; the row clears on a config edit plus a restart, or when the 60 s usage poll re-probes a healed credential*

The footer drops `esc` - there is nothing left to cancel - and the paths are printed in full. A command too long for the panel wraps after a `/` onto a deeper-indented continuation line; it is never elided.

<div class="term">

  <pre class="indent">
      <span class="dim">────────────────────────────────────────────────────────</span>
        <span class="dim bold">Accounts</span>
        <span style="color:#7eb87a">●</span> Subspace                              <span class="dim">       ready</span>
        <span style="color:#7eb87a">●</span> Granite                               <span class="dim">       ready</span>
        <span style="color:#cf6171">⚠</span> <span class="bold">Granite1</span>                              <span style="color:#cf6171"> auth failed</span>
        <span style="color:#7eb87a">●</span> Personal                              <span class="dim">       ready</span>
        <span style="color:#7eb87a">●</span> Codex                                 <span class="dim">       ready</span>

        <span class="dim bold">Dictation</span>
        <span style="color:#7eb87a">●</span> transcribing model                    <span class="dim">       ready</span>
          <span class="dim">(cohere-transcribe-03-2026-Q4_K_M)</span>
        <span style="color:#7eb87a">●</span> normalization model                   <span class="dim">       ready</span>
          <span class="dim">(s1-mini-f16)</span>

        <span class="error">Granite1 will not start a session. forge starts</span>
        <span class="error">without it; fix the auth and restart forge to pick</span>
        <span class="error">the re-mint up.</span>

        <span class="bold">Fix the auth</span>
          CLAUDE_CODE_OAUTH_TOKEN in [accounts.env]
          claude setup-token
          <span class="dim">editing [accounts.env] needs a restart</span>

        <span class="bold">Or drop the account</span>
          delete its [[accounts]] block from
          ~/.claude/forge/forge.toml
      <span class="dim">────────────────────────────────────────────────────────</span>

 <span class="dim">ctrl+q  quit</span></pre>

</div>

An account whose `provider` is a base-url one (`codex`, `openrouter` or `zai`) has its credential in the `ANTHROPIC_AUTH_TOKEN` beside its base url. An `anthropic` account's credential is its setup token - `CLAUDE_CODE_OAUTH_TOKEN`, from its `[accounts.env]` or the global `[env]` - and its repair is a mint or re-mint. Re-authenticating the config dir is never the repair: it would authenticate whichever account owns the shared dir, not the one that failed. The branch is on the account class, not on the presence of `ANTHROPIC_BASE_URL`, so an `anthropic` account that happens to set one still gets token copy:

<div class="term">

  <pre class="indent">
        <span class="bold">Fix the auth</span>
          ANTHROPIC_AUTH_TOKEN in [accounts.env]
          <span class="dim">editing [accounts.env] needs a restart</span>

        <span class="bold">Or drop the account</span>
          delete its [[accounts]] block from
          ~/.claude/forge/forge.toml</pre>

</div>

<div class="term">

  <pre class="indent">
        <span style="color:#cf6171">TokenAcct will not start a session. forge starts</span>
        <span style="color:#cf6171">without it; fix the auth and restart forge to pick</span>
        <span style="color:#cf6171">the re-mint up.</span>

        <span class="bold">Fix the auth</span>
          CLAUDE_CODE_OAUTH_TOKEN in [accounts.env]
          claude setup-token
          <span class="dim">editing [accounts.env] needs a restart</span>

        <span class="bold">Or drop the account</span>
          delete its [[accounts]] block from
          ~/.claude/forge/forge.toml</pre>

</div>

A token-mode account that is valid never reaches this screen at all: the usage endpoint refuses a setup token (it lacks the `user:profile` scope), so the token arm probes a minimal billed messages call instead and its response headers carry the 5-hour and 7-day windows. A 401 is a genuinely rejected token, and only that bails.

An endpoint that never answered is a different failure, and the row says so: the last probe attempt classified the error, and an endpoint-class bail reads `unreachable` or `fetch error` (a proxy with a dead upstream 502s rather than refusing), with the repair aimed at the endpoint rather than the token. A `rate limited` bail has no repair at all beyond waiting: the pollers keep retrying. forge does not hold boot for any of them:

<div class="term">

  <pre class="indent">
      <span class="dim">────────────────────────────────────────────────────────</span>
        <span class="dim bold">Accounts</span>
        <span style="color:#7eb87a">●</span> Subspace                              <span class="dim">       ready</span>
        <span style="color:#cf6171">⚠</span> <span class="bold">Granite1</span>                              <span style="color:#cf6171">  unreachable</span>
        <span style="color:#7eb87a">●</span> Personal                              <span class="dim">       ready</span>
        <span style="color:#7eb87a">●</span> Codex                                 <span class="dim">       ready</span>

        <span class="error">Granite1 cannot be reached. forge starts without</span>
        <span class="error">it and keeps retrying.</span>

        <span class="bold">Check the endpoint</span>
          ANTHROPIC_BASE_URL in [accounts.env], or the
          endpoint itself
          <span class="dim">editing forge.toml needs a restart; fixing the</span>
          <span class="dim">endpoint does not</span>

        <span class="bold">Or drop the account</span>
          delete its [[accounts]] block from
          ~/.claude/forge/forge.toml
      <span class="dim">────────────────────────────────────────────────────────</span></pre>

</div>

## Preflight fails: a model will not verify

*the crate reports a mismatched file rather than repairing it*

Both digests are cut to twelve hex characters a side. A size mismatch is a different error and gets the crate's own wording, which reads as a truncated download rather than as corruption.

<div class="term">

  <pre class="indent">
      <span class="dim">────────────────────────────────────────────────────────</span>
        <span class="dim bold">Dictation</span>
        <span style="color:#7eb87a">●</span> transcribing model                    <span class="dim">       ready</span>
          <span class="dim">(cohere-transcribe-03-2026-Q4_K_M)</span>
        <span style="color:#cf6171">⚠</span> <span class="bold">normalization model</span>                   <span style="color:#cf6171">    bad hash</span>
          <span class="dim">(s1-mini-f16)</span>

        <span class="error">s1-mini-f16.gguf hashes to</span>
          <span class="bold">4f2b9c1a77e0</span>
        <span class="error">expected</span>
          <span class="bold">0370da4f1bae</span>

        It is the right length, so this is corruption and
        not a half-finished download. forge reports it
        rather than deleting it: throwing away a 1.51 GB file you
        put there is not forge's call.

        <span class="bold">Delete it and forge fetches it again</span>
          rm ~/Library/Caches/forge-dictate/
             s1-mini-f16.gguf
      <span class="dim">────────────────────────────────────────────────────────</span>

 <span class="dim">ctrl+q  quit</span></pre>

</div>

## First run, resume, and cancel

*3.07 GB once, resumable, cancellable throughout*

A fresh fetch carries the note saying what it is about to move and where it keeps what lands; a resume does not. The resume line names the byte count it found. **Cancelling quits forge** - there is no dictation-less runtime to fall back to - so the screen says what it kept and where before it goes, and only quits once that frame has been painted.

<div class="term">

  <pre class="indent">
        <span class="dim bold">Dictation</span>                              <span class="dim"># a first run</span>
        <span class="rust-orange">⠹</span> transcribing model                    <span class="dim"> downloading</span>
          <span class="dim">(cohere-transcribe-03-2026-Q4_K_M)</span>
          <span class="rust-orange">████</span><span class="dim">░░░░░░░░░░░░░░░░░░░░░░</span><span class="dim">   14%  218 MB / 1.56 GB</span>
        <span class="rust-orange">⠹</span> normalization model                   <span class="dim"> downloading</span>
          <span class="dim">(s1-mini-f16)</span>
          <span class="rust-orange">███</span><span class="dim">░░░░░░░░░░░░░░░░░░░░░░░</span><span class="dim">   11%  166 MB / 1.51 GB</span>

        First run fetches 3.07 GB once. Quitting keeps
        what has landed, in ~/Library/Caches/forge-dictate.

 <span class="dim">esc  cancel and quit     ctrl+q  quit</span>

        <span class="dim bold">Dictation</span>                              <span class="dim"># resumed across a restart</span>
        <span class="rust-orange">⠹</span> transcribing model                    <span class="dim">    resuming</span>
          <span class="dim">(cohere-transcribe-03-2026-Q4_K_M)</span>
          <span class="rust-orange">██████████</span><span class="dim">░░░░░░░░░░░░░░░░</span><span class="dim">   38%  592 MB / 1.56 GB</span>
          <span class="dim">resumed from 592 MB found in .part</span>

 <span class="dim">esc  cancel     ctrl+q  quit</span>

        <span class="dim bold">Dictation</span>                              <span class="dim"># esc pressed</span>
        <span style="color:#cf6171">⚠</span> <span class="bold">transcribing model</span>                    <span style="color:#cf6171">   cancelled</span>
          <span class="dim">(cohere-transcribe-03-2026-Q4_K_M)</span>
          <span class="rust-orange">██████████████████████████</span><span class="dim"></span><span class="dim">  612 MB</span>
        <span class="rust-orange">○</span> normalization model                   <span class="dim"> downloading</span>
          <span class="dim">(s1-mini-f16)</span>
          <span class="rust-orange">████████</span><span class="dim">░░░░░░░░░░░░░░░░░░</span><span class="dim">   31%  468 MB / 1.51 GB</span>

        Nothing was thrown away. 612 MB is on disk as a
        .part file and the next run resumes from there.

        <span class="bold">forge is quitting.</span></pre>

</div>

- **config** - `[dictate]` in `forge.toml`, off unless asked for: `enabled`, `models_dir`, `device`, `language`, `normalizer` (a bool - off halves the download and skips a pass per utterance), `max_capture_minutes`. An unknown key fails the load rather than being ignored. The model specs and the normalizer's three prompt axes stay internal - see **the section intro** and the crate docs.
- **costs** - 3.07 GB on a first run, resumable via `.part` and SHA-256 verified. Every later run re-hashes both files end to end, about 2.7 s for the pair now the two hash concurrently, then loads the weights: 1.0 s warm, 7 s on a cold page cache. About 1.8 GB of physical footprint is then held for the run, which is what Activity Monitor reports against forge.
- **cancel** - `ControlFlow::Break` out of the progress callback, surfacing as `Error::Cancelled`. Whatever reached `ready` stays installed and every in-flight `.part` is left where it is, so the next run resumes. Both models run at once, so a cancel can leave TWO partials while the screen's byte counts name only the transfer it is reported against; the other model's own row still carries its bar.
- **cancel is not instant** - `verify()` hashes with no progress callback, so a `Break` is not seen until the hash it interrupted finishes - up to about 2.6 s on the shipped pair, during which the rows keep their last state and the footer still reads `esc  cancel and quit`. That predates the models being prepared concurrently and is [#799](https://github.com/busytools/forge/issues/799); what concurrency changed is that a cancel landing while one model downloads and the other verifies now waits for that verify, where serially the second model had not started.
