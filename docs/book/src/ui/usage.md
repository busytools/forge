# Usage - token/cost view

## Full-screen usage overlay (`/usage`)

A pinned summary header - lifetime tokens and one notional cost, today / week / month quick totals, and the input / cache-write / cache-read / output split - over a scrollable table of the same data grouped by project (default) or by model (<kbd>g</kbd> flips it), for any window (<kbd>w</kbd> cycles today · week · month · lifetime, default lifetime). Rows sort by cost descending; a pinned `TOTAL` row and key hints sit at the bottom; <kbd>↑↓</kbd> / <kbd>PgUp/Dn</kbd> scroll the full list - every project reachable, no collapse. <kbd>Esc</kbd> closes; changing group or window returns the scroll to the top.

The table is responsive: the label column fits the longest name (full model ids and long project names never truncate when they fit) and the six numeric columns spread across the remaining width. The notional caption sits right-justified on the summary's first row.

All accounts pool into one aggregate (the per-account config dirs share one projects pool, so per-account is not separable), deduped by message id; worktree and sub-path slugs fold into their parent repo and `/tmp` folds to a single **scratch** bucket. The dollar column is **notional** - at API pricing, not a bill; GPT/Codex rows are flagged `(GPT approx)`; an unpriced model shows its tokens with a `-` cost.

Pricing is fetched at runtime and cached (no bundled table) - the first open with an empty cache shows tokens with a blank cost until the fetch lands, then re-prices; the cache refreshes about once a day. The scan is cached incrementally per file, so reopening is fast. While no pricing is loaded the header caption switches to a yellow `pricing pending or failed` and the LIFETIME headline cost blanks to `-`, so an unpriced report never shows a misleading `$0.00`.

<div class="term">

  <pre>
<span class="dim">┌─ Usage ────────────────────────────────────────────────────────────────────────────────────────────────┐</span>
<span class="dim">│</span><span class="bold">  LIFETIME  </span><span class="bold">680.5M</span><span class="dim"> tokens · </span><span class="accent-bold">$320.00</span><span class="accent">≈</span><span class="dim">   17 projects · 4 models</span>     <span class="dim">notional · at API pricing · not a bill</span><span class="dim">│</span>
<span class="dim">│</span>  <span class="dim">────────────────────────────────────────────────────────────────────────────────────────────────────</span>  <span class="dim">│</span>
<span class="dim">│</span>  <span class="dim">this month </span>142.0M  <span class="accent">$71.00</span>                   <span class="dim">this week </span>38.0M  <span class="accent">$19.00</span>                  <span class="dim">today </span>8.2M  <span class="accent">$4.10</span><span class="dim">│</span>
<span class="dim">│</span><span class="dim">  split   input </span>3.9M    <span class="warning">cache-write </span><span class="warning">26.6M    </span><span class="success">cache-read </span><span class="success">648.0M </span><span class="success">(95%)</span><span class="dim">    output </span>1.9M                     <span class="dim">│</span>
<span class="dim">│</span>  <span class="dim">────────────────────────────────────────────────────────────────────────────────────────────────────</span>  <span class="dim">│</span>
<span class="dim">│</span><span class="dim">  group:  </span><span class="dim">by model</span><span class="dim">  ·  </span><span class="accent-bold">by project</span><span class="dim">      window: </span><span class="dim">today · week · month · </span><span class="accent-bold">lifetime</span>                          <span class="dim">│</span>
<span class="dim">│</span>  <span class="dim">────────────────────────────────────────────────────────────────────────────────────────────────────</span>  <span class="dim">│</span>
<span class="dim">│</span>  <span class="dim">PROJECT         </span><span class="dim">          INPUT</span><span class="dim">       CACHE·wr</span><span class="dim">      CACHE·rd</span><span class="dim">        OUTPUT</span><span class="dim">        TOKENS</span><span class="dim">         COST</span><span class="accent">≈</span><span class="dim">│</span>
<span class="dim">│</span>  <span class="dim">────────────────────────────────────────────────────────────────────────────────────────────────────</span>  <span class="dim">│</span>
<span class="dim">│</span>  <span class="bold">forge           </span><span class="dim">           1.8M</span><span class="warning">          12.0M</span><span class="success">        228.0M</span><span class="dim">         0.90M</span><span class="bold">        242.7M</span><span class="accent-bold">       $115.00</span><span class="dim">│</span>
<span class="dim">│</span>  <span class="bold">web-api         </span><span class="dim">           0.6M</span><span class="warning">           4.2M</span><span class="success">        108.0M</span><span class="dim">         0.40M</span><span class="bold">        113.2M</span><span class="accent">        $55.00</span><span class="dim">│</span>
<span class="dim">│</span>  <span class="bold">stargate        </span><span class="dim">           0.4M</span><span class="warning">           3.0M</span><span class="success">         82.0M</span><span class="dim">         0.28M</span><span class="bold">         85.7M</span><span class="accent">        $42.00</span><span class="dim">│</span>
<span class="dim">│</span>  <span class="dim">scratch         </span><span class="dim">          0.30M</span><span class="warning">           1.9M</span><span class="success">         52.0M</span><span class="dim">         0.30M</span><span class="bold">         54.5M</span><span class="dim">        $26.00</span><span class="dim">│</span>
<span class="dim">│</span>  <span class="bold">data-modules    </span><span class="dim">          0.12M</span><span class="warning">           0.9M</span><span class="success">         22.0M</span><span class="dim">         0.09M</span><span class="bold">         23.1M</span><span class="accent">        $11.00</span><span class="dim">│</span>
<span class="dim">│</span>  <span class="dim">────────────────────────────────────────────────────────────────────────────────────────────────────</span>  <span class="dim">│</span>
<span class="dim">│</span>  <span class="accent-bold">TOTAL           </span><span class="bold">           3.9M</span><span class="warning">          26.6M</span><span class="success">        648.0M</span><span class="bold">         1.90M</span><span class="bold">        680.5M</span><span class="accent-bold">       $320.00</span><span class="dim">│</span>
<span class="dim">│</span>  <span class="dim">────────────────────────────────────────────────────────────────────────────────────────────────────</span>  <span class="dim">│</span>
<span class="dim">│</span><span class="dim">  ↑↓ </span><span class="accent">scroll</span><span class="dim">  ·  g </span><span class="accent">group</span><span class="dim">  ·  w </span><span class="accent">window</span><span class="dim">  ·  Esc </span><span class="accent">close</span>                                                      <span class="dim">│</span>
<span class="dim">└────────────────────────────────────────────────────────────────────────────────────────────────────────┘</span></pre>

</div>

Grouping by model swaps the table in place - full model ids render without truncation and the label column widens to fit:

<div class="term">

  <pre>
<span class="dim">┌─ Usage · by model ─────────────────────────────────────────────────────────────────────────────────────┐</span>
<span class="dim">│</span><span class="dim">  group:  </span><span class="accent-bold">by model</span><span class="dim">  ·  </span><span class="dim">by project</span><span class="dim">      window: </span><span class="dim">today · week · month · </span><span class="accent-bold">lifetime</span>                          <span class="dim">│</span>
<span class="dim">│</span>  <span class="dim">MODEL             </span><span class="dim">         INPUT</span><span class="dim">      CACHE·wr</span><span class="dim">      CACHE·rd</span><span class="dim">        OUTPUT</span><span class="dim">        TOKENS</span><span class="dim">         COST</span><span class="accent">≈</span><span class="dim">│</span>
<span class="dim">│</span>  <span class="dim">────────────────────────────────────────────────────────────────────────────────────────────────────</span>  <span class="dim">│</span>
<span class="dim">│</span>  <span class="bold">claude-opus-4-8   </span><span class="dim">          2.1M</span><span class="warning">         18.0M</span><span class="success">        430.0M</span><span class="dim">         1.10M</span><span class="bold">        451.2M</span><span class="accent-bold">       $240.00</span><span class="dim">│</span>
<span class="dim">│</span>  <span class="bold">claude-sonnet-4-5 </span><span class="dim">          0.8M</span><span class="warning">          5.4M</span><span class="success">        120.0M</span><span class="dim">         0.40M</span><span class="bold">        126.6M</span><span class="accent">        $58.00</span><span class="dim">│</span>
<span class="dim">│</span>  <span class="bold">gpt-5-codex       </span><span class="dim">          0.4M</span><span class="dim">             -</span><span class="dim">             -</span><span class="dim">         0.20M</span><span class="bold">          0.6M</span><span class="experimental">         $3.60</span><span class="dim">│</span>
<span class="dim">│</span>  <span class="dim">&lt;synthetic&gt;       </span><span class="dim">          0.1M</span><span class="dim">             -</span><span class="success">          6.0M</span><span class="dim">         0.05M</span><span class="bold">          6.2M</span><span class="dim">             -</span><span class="dim">│</span>
<span class="dim">│</span>  <span class="dim">────────────────────────────────────────────────────────────────────────────────────────────────────</span>  <span class="dim">│</span>
<span class="dim">│</span>  <span class="accent-bold">TOTAL             </span><span class="bold">          3.9M</span><span class="warning">         26.6M</span><span class="success">        648.0M</span><span class="bold">         1.90M</span><span class="bold">        680.5M</span><span class="accent-bold">       $320.00</span><span class="dim">│</span>
<span class="dim">└────────────────────────────────────────────────────────────────────────────────────────────────────────┘</span></pre>

</div>

| Key | Action |
|---|---|
| <kbd>g</kbd> | Toggle grouping (project ⇄ model) |
| <kbd>w</kbd> | Cycle window (today · week · month · lifetime) |
| <kbd>↑↓</kbd> / <kbd>PgUp/Dn</kbd> | Scroll the full list |
| <kbd>Esc</kbd> | Close |

| Cell or element | Color |
|---|---|
| `cache-write` column | yellow |
| `cache-read` column | green |
| `TOKENS` column | bold |
| Cost column | rust orange (the `TOTAL` cost accent-bold); GPT/Codex rows amber |
| Unpriced `-` | dim |
| Active group / window in the selector | rust orange bold; inactive dim |
| `scratch` and `<synthetic>` row labels, rules, column labels | dim |
| Key hints | dim labels, rust-orange action words |

<details>
<summary>Data source and scope</summary>

The scan reads the one real `~/.claude/projects` JSONL pool off the render thread (canonicalized so the symlinked per-account dirs resolve to it once; Syncthing conflict copies skipped), sums each assistant record's usage per model and day deduped by message id, folds each slug to its repo, and rolls the windows against the cached pricing table. Scope: the summary, both groupings and windows over the deduped pool; per-account breakdown is impossible (shared pool, no per-account tag), and a live burn-rate strip (tokens/min across live agents, exhaustion projection) is deferred to phase 2.

</details>
