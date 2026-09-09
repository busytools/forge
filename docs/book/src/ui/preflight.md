# Preflight - the launchpad's first view

The first thing forge renders on every route: it resolves the accounts and, when dictation is on, fetches and loads its models, then hands over - to the projects view for `forge`, straight into chat for `forge <project>`. Shown once per run; later `/launchpad` goes straight to the projects view. Nothing spawns until every account settles - the assignment plan is computed only then, so a session started before it could land on an account the project's org does not allow. Preflight completes when every account settles, not only when all are Ready: a bailed account is degraded rather than holding boot, and its row names the failure. Every failure state names its exits; repairing an account's auth means editing `forge.toml`, which needs a restart - the screen states the restart without a number.

## Preflight, resolving

Two sibling sections at the same indent, one row shape: two-cell indent, state glyph, name, right-aligned twelve-cell state column. The same panel width the projects view uses, so the hand-over is a content swap. Model rows read by role with the file on a dim continuation line beneath. No summary line.

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

| State | Glyph | Meaning |
|---|---|---|
| `resolving` | `○` yellow | probe in flight |
| `ready` | `●` green | probe returned |
| `auth failed` | `⚠` red | rejected or expired credentials |
| `unreachable`, `fetch error`, `rate limited` | `⚠` yellow | transient failures the pollers heal (`fetch error` is a classed-but-unrecognised failure: a 5xx proxy, a body that will not decode, a 200 mapping to nothing) |
| Model states | | `queued`, `downloading`, `resuming` (picked up a `.part`), `verifying`, `ready`, `loading`, then `ready`; a failure reads `bad hash` or `cancelled`; a row nothing will now start reads `not started` |

- The spinner is the configured `[ui] spinner` style at its own cadence, shared with every other animated surface.
- <kbd>Esc</kbd> cancels an in-flight model download, which quits forge; <kbd>Ctrl+Q</kbd> quits. Every other key is consumed silently - the projects view underneath is not reachable yet.
- The hand-over is latched: a mid-session Ready → Bailed → Loading flip never throws you back onto this screen - the launchpad's own gate covers the window.
- The wordmark is dropped before any panel content when the block does not fit - the failure exits are the one thing this screen cannot clip. Past that rows drop from the TOP, replaced with a dim `… N more above`; the failure detail is appended last and never vanishes unmarked (at 100x24 the bailed screen already overflows).
- Verifying draws a spinner and no byte counter - hashing checkpoints cancellation rather than reporting progress.
- The two models prepare concurrently; the pair costs the slower of the two.

## Preflight fails: a bailed account

Rides along as degraded - the row clears on a config edit plus a restart, or when the poll re-probes a healed credential. The footer drops `esc` and the paths print in full; a command too long for the panel wraps after a `/` onto a deeper-indented continuation, never elided.

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

<details>
<summary>Bail classes and repair paths</summary>

- A base-url account (`codex`, `openrouter` or `zai`) keeps its credential in `ANTHROPIC_AUTH_TOKEN` beside its base url. An `anthropic` account's credential is its setup token - `CLAUDE_CODE_OAUTH_TOKEN`, from its `[accounts.env]` or the global `[env]` - and its repair is a mint or re-mint. Re-authenticating the config dir is never the repair: it would authenticate whichever account owns the shared dir, not the one that failed. The branch is on the account class, not on whether `ANTHROPIC_BASE_URL` is set.
- A token-mode account that is valid never reaches this screen: the usage endpoint refuses a setup token (it lacks the `user:profile` scope), so the token arm probes a minimal billed messages call whose response headers carry the 5-hour and 7-day windows. A 401 is a genuinely rejected token, and only that bails.
- An endpoint that never answered reads `unreachable` or `fetch error`, the repair aimed at the endpoint rather than the token; a `rate limited` bail has no repair beyond waiting - the pollers keep retrying. forge does not hold boot for any of them.
- Model verify failure: both digests are cut to twelve hex characters a side; a size mismatch is a different error with the crate's own wording, reading as a truncated download rather than corruption.

</details>

The token-path screens:

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

An endpoint that never answered is a different failure, and the row says so:

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

The crate reports a mismatched file rather than repairing it:

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

A fresh fetch carries the note saying what it is about to move and where it keeps what lands; a resume names the byte count it found. **Cancelling quits forge** - there is no dictation-less runtime to fall back to - so the screen says what it kept and where before it goes, quitting only once that frame is painted.

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

<details>
<summary>Dictation config, costs, and cancel mechanics</summary>

- 3.07 GB on a first run, resumable via `.part` and SHA-256 verified. Every later run re-hashes both files (about 2.7 s for the pair, concurrent) then loads the weights: 1.0 s warm, 7 s on a cold page cache. About 1.8 GB of physical footprint is held for the run.
- `[dictate]` in `forge.toml`, off unless asked for: `enabled`, `models_dir`, `device`, `language`, `normalizer` (a bool - off halves the download and skips a pass per utterance), `max_capture_minutes`. An unknown key fails the load rather than being ignored. The model specs and the normalizer's prompt axes stay internal.
- Cancel: whatever reached `ready` stays installed and every in-flight `.part` is left where it is, so the next run resumes. Both models run at once, so a cancel can leave TWO partials while the screen's byte counts name only the transfer they are reported against; the other model's own row still carries its bar.
- Cancel is not instant: verifying hashes with no progress callback, so a cancel is not seen until the hash it interrupted finishes - up to about 2.6 s on the shipped pair, during which the rows keep their last state and the footer still reads `esc  cancel and quit` ([#799](https://github.com/busytools/forge/issues/799)).

</details>
