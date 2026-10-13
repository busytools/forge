# Usage

The page that shows what this forge's sessions have spent: the lifetime
headline with its notional cost, the three rolling totals, the split across
input, cache write, cache read and output, and a table of the same figures
for any of four windows, grouped by project or by model. It is drawn from
one snapshot of the `usage` subject - the four windows in full - and from
nothing else. The drawing it is held against is
[web-usage.html](./web-usage.html), beside this page.

It lives at `/usage`, which is its own address rather than a session's: the
report is the pool's, belongs to no seat, and the subject carries no slot.
The header's way back is the home. **The door is in the system band**, with
server, dictation and extensions: the usage card is a way in rather than a
read - the report is a scan of the transcript pool, and the home is not the
page that should pay for one.

**The subject scans on the ask and hears nothing after it.** The pool
changes as transcripts are written, and nothing in the stream says a
transcript's tokens moved, so a page that waited for an update would wait
for ever. Fresh figures are the refresh control, and the server re-scans
after its held window lapses. **The unpriced report is the one case the page
asks about by itself**: the scan that answered also kicked the server's
pricing fetch, so while the report says no pricing table stood behind it the
page re-asks - every six seconds, four times, which reaches past the
fetch's own 20-second budget - and stops the moment a priced report lands or
the count is spent. A fetch that failed stops there rather than looping:
`pricing pending or failed` is a terminal caption.

The rules below are the page's own: they were the overlay's vocabulary
once, and this page is where that reading lives now.

## What it draws

| Region | Shows | Read from |
|---|---|---|
| Header | the brand mark, `forge`, the page's name, and the way back to the home | `ClientSettings.mark` from the greeting; the route |
| The summary | the lifetime headline: tokens, the notional cost with its `≈`, the project and model counts, and the caption; below it the three rolling totals - this month, this week, today - with tokens and cost each; below them the split: input, cache-write, cache-read with its share, output | `lifetime`, `month`, `week`, `today` |
| The controls | the grouping pick (by project, by model), the window pick (today, week, month, lifetime), and refresh | the route's own state; `Connection::refresh('usage')` |
| The table | one row per project or model for the chosen window - input, cache write, cache read, output, their token sum, and the cost - with the `TOTAL` pinned as the table's own footer | `by_project` / `by_model` and `total` of the chosen window |

## What the figures mean

**The scales.** Tokens draw in B, M or K at one decimal,
truncated rather than rounded up, so a figure never reads larger than it is;
anything under a thousand draws as it is.

**A cost with no price behind it is a dash, not a zero.** `pricing_available`
false banks the headline cost to `-`, and every unpriced row draws `-` in
the dim tone; the caption says which of the two states it is: `notional ·
at API pricing · not a bill` in dim while a table stood behind the figures,
`pricing pending or failed` in the warning tone while none did. The `≈` on
the headline figure is the caption stated as a mark.

**The cache share is over the whole denominator.** The split's percentage is
cache-read against all five fields summed - input, both cache-write fields,
cache-read, output - floored to a whole percent, which is this page's own
arithmetic. Zero lifetime tokens have no share and draw none.

**The GPT flag rides the MODEL grouping only.** A row whose label starts
`gpt` or contains `codex`, when the table is grouped by model, is flagged
`(GPT approx)` beside its cost and draws that cost in amber - the price is
an approximation, and the flag is a fact about the model row. A project that
merely reads GPT-shaped is not a model and is never flagged, and the TOTAL
row never is.

**The pooled labels draw quiet.** `scratch` (where `/tmp`'s sessions fold)
and `<synthetic>` name places rather than projects, and the page dims
them.

**The rows arrive sorted, and the total is pinned.** The server's scan sorts
each grouping by cost descending with the label as the tiebreak; the page
draws the order it is given, and the window's `total` sits in the table's
footer row rather than among the rows.

**The headline is always the lifetime.** The window pick moves the table
only; the headline, the rolling totals and the split are the lifetime's
whatever the table reads, which is the overlay's shape.

## The states it draws

- **Before the first read**: `Reading the usage...`, and the door when no
  server has answered at all.
- **A refused subscription**: the server's own words, with the door not
  redrawn - the address is not what is wrong.
- **An empty pool**: `No usage yet`, its own page rather than a headline of
  zeros over an empty table, which reads as a report that failed.
- **An unpriced report**: the tokens with the dashes and the warning
  caption, above; the page's re-ask draws the priced figures over them when
  the fetch lands.

## The door

`/usage` opens the page, from the home band's card, from the address, and
from the seat: **a typed `/usage` is answered in the view before it reaches
the wire** - the name opens the page without a round trip - and works from
the composer and from the command palette's row
alike. A name given arguments takes no arguments, so `/usage today` answers
`Usage: /usage` in the composer's own notice row rather than opening or
reaching the model.

## The pieces it takes from the pages beside it

- **The header**, and its title-and-back chrome: the shared
  `header.top.pagetop` block, which the models and extensions pages read
  too rather than a third copy of the same rules.
- The brand lockup and the `.wrap` column, from the home.
- The band card's ground - the gradient over `--line` - as the summary's
  surface.
- The table idiom from the models page's comparison table: mono figures,
  the hairline between rows, a right-aligned numeric column.
- The state marks and the dim treatment for a pooled label, from the shared
  vocabulary.

## The two widths

The page is checked at **1600** and at **430**, against the drawing beside
it. The sheet admits no sideways scroller, so the table never scrolls: a
long model id breaks inside its cell at narrow widths rather than pushing
the page wide, and the summary's rows wrap onto their own lines.

## Touch

Every control takes the 44px target under `@media (pointer: coarse)` - the
shared rule covers the pick chips and the refresh control, which are
buttons, and the page adds the one it cannot reach: the way back to the
home. No affordance lives on hover alone.
