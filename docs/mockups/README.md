# Web view mockups

Mockups for forge's web view. Each file is one screen, written as a
standalone HTML fragment: it is meant to be dropped into a page that
supplies its own frame, so no file carries a doctype or a body tag.

## The approved direction

`web-main-a-crafted.html` is the direction that was approved, and it is
the visual target the implementation work builds against. It keeps the
three regions and the account panel and lays them out for a browser
rather than for a character grid.

Two further directions were drawn for comparison and were not taken up:

- `web-main-b-dashboard.html` - the same content as a card dashboard.
- `web-main-c-workbench.html` - the conversation driving a work surface
  on the left, the session strip on the far left.

`web-diff-review.html` is the git diff and review flow. It is drawn
against components the web ecosystem already has rather than a
hand-built diff: a maintained diff view for the hunks and their
word-level highlighting, a virtualised list for the file rail, and a
markdown renderer for comment bodies. The caption under the frame names
the candidates.

## The twelve surfaces

One file per surface the TUI renders today, each arranged the way it
renders today.

| File | Surface |
|---|---|
| `inspector-tabs.html` | the inspector pane with its sections as tabs |
| `inspector-section-detail.html` | those sections in detail |
| `diff-review-overlay.html` | the full-frame diff viewer |
| `pickers-and-overlays.html` | the picker overlays and the narrow-tier panes |
| `usage-page.html` | the usage page and the account panel |
| `extensions-page.html` | the extensions page |
| `launchpad-view.html` | the project picker |
| `preflight-dictation.html` | the boot screen's dictation rows and the device pick |
| `help-and-welcome.html` | the help overlay and the welcome block |
| `agents-surface.html` | agent messaging blocks and the peer badges |
| `slack-connector.html` | Slack subscriptions, a delivered block, the approval dock |
| `reference-glyphs.html` | theme tokens, the glyph inventory, notices |

## Two things that cost time to rediscover

- The files are ASCII, with every glyph written as an HTML entity.
  Served without a charset declaration the literal box-drawing
  characters arrive as mojibake; entities are immune to that, and they
  also keep the punctuation gate happy.
- The scheduled-wakeup row uses `◷` (U+25F7) where the TUI uses the
  alarm clock (U+23F0). A browser paints U+23F0 as a colour emoji
  whatever the CSS colour says, and neither `font-variant-emoji: text`
  nor a VS15 suffix changes that; U+25F7 stays monochrome and takes the
  colour. Two other glyphs are emoji-only in a browser and stay in
  colour: the comment balloon (U+1F4AC) and the owl (U+1F989). The
  hourglass (U+231B) is fixed by a VS15 suffix.

## Removal

These come out of the repo when the web view migration lands. They are a
design record and not a description of shipped behaviour; the book's
`ui/` pages stay the source of truth for what forge renders.
