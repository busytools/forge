# Connect

The client's first screen and the only one without a drawing behind it,
so it is also the only one where a choice is a decision rather than a
port. The drawing beside this page is
[web-connect.html](./web-connect.html), and the home it leads to is
[Home](./home.md).

## The direction, chosen

Four were drawn and Ved picked this one, 2026-09-29: *"I kind of like the
webconnect.html vibe more than the prompt spine, or type. Let's stick
with that webconnect.html."* The other three are kept beside it as the
record of the choice, the way `docs/mockups/web-logo-options-2.html`
keeps the mark's.

| Direction | Kept as | Why it lost |
|---|---|---|
| `web-connect-prompt.html` | the record | its own vernacular was the point, and it read as a shell pretending to be a form once a button had to be reachable by pointer as well as by Enter |
| `web-connect-spine.html` | the record | the three-step block is honest but it spends the screen's whole top on a process where the common case is one press |
| `web-connect-type.html` | the record | the most minimal of the four, and the address at the sheet's largest step is thinner than the wordmark beside it, so the page has no first thing to read |

## What it is for

One developer, on their own machine, who has either just started `forge`
or has not. The screen's job is to get them to a running forge in one
press when the default is right, which is almost always, and to give a
legible reason when it is not.

**The common case is not reaching it at all.** The app keeps the last
address that answered and opens on it, so this screen is what a first
launch meets and what the launch meets when the remembered address does
not answer - and in that second case it opens with that address already
in the field and the failure already on it, rather than blank. A failed
launch is not forgotten, either: the address stays remembered, because
that is the one the reader is about to correct.

**A launch moves the page only from the root**, so a deep link is the one
place it takes the connection and leaves the route alone. That makes this
screen reachable three ways: a first launch, a launch that did not answer,
and navigating here. On its own `/connect` the door is already drawn when
the attempt lands, so the reason arrives on it rather than opening it.
**A submit wins over a launch, whichever of the two lands first.** A launch
that lands after a submit has taken a connection yields rather than closing
the socket that submit opened, and a launch that lands before it is replaced
by the submit when it takes. Either way the page draws the forge the reader
asked for and not the one the app was guessing at.

## The design plan

**Colour.** The theme's own tokens, unchanged: `--bg`, `--s1`, `--line`,
`--text`, `--muted`, `--dim`, `--accent`, `--bad`. **This screen carries
no palette of its own**, because one theme is the project's hard scope
rule and the server owns the palette. That is a decision rather than a
constraint worked around: a first screen is exactly where an identity is
usually invented, and here the identity is already the mark and the
palette every later page draws with.

**Type.** Inter for prose and the button, Fira Code for the address. The
address is data, and this house sets data in the mono face.

**Layout.** One centred column, four rows, all left-aligned to the
column's edge.

```
  |  mark forge
  |
  |  127.0.0.1:8790                          mono, 18px, editable in place
  |  ----------------------------------      hairline: accent on focus, bad on failure
  |  the port forge serves on by default      dim, one line
  |
  |  [ Connect ]
  |
  |  this app draws a forge running elsewhere  dimmest
```

**Principles.**

1. **The address is the headline, not a form field.** An address is what
   this screen is about. Set as the largest thing in the mono face and
   edited in place, it reads as the subject; a bordered input would read
   as paperwork.
2. **Nothing moves between states.** Only the hairline's colour, the line
   under it, and the button's label change. The reader's eye stays where
   it already was, which is what a screen with three states needs.
3. **One action.** Editing the address in place is the whole of the
   uncommon case, so there is no second control.
4. **No decoration that is not the theme's.** The mark is the only
   graphic, and it is the same one every page draws.

## The three states

| State | The underline | The line under it | Button |
|---|---|---|---|
| Idle | `--line` | the port forge serves on by default | Connect |
| Connecting | an accent segment running its length | asking this address for its projects | Connecting, disabled |
| Failed | `--bad` | why nothing answered, and what to check | Try again |

**The underline is one element doing two jobs**, which is why no
second graphic appears in the connecting state: it is already the line
under the address, so it carries the progress rather than a spinner
being added beside it. Under `prefers-reduced-motion` it stops moving
and goes solid, which says the same thing without motion.

**The failed line names the address it tried**, and when nothing answered
it points at `[web] enabled` in `forge.toml`: a forge whose owner turned
the socket off refuses in silence, with nothing wrong at either end, and
a generic "could not connect" sends the reader off to look at their
network.

**A forge that answers and speaks a protocol this client does not is a
failure of its own**, with its own line. The connection was made, so the
unreachable hint would send the reader to look at their network and their
`forge.toml` for a problem that is neither; the only thing that fixes a
mismatch is the two halves agreeing.

## What the first pass got wrong

The first attempt drew the mark large and centred with the address in a
boxed field below it, and a "use a different address" link that revealed
the field on demand. Both parts are the landing-page reflex: a big logo,
then progressive disclosure of a form. The subject is a one-field
utility, not a landing page, so the address took the hero slot and the
field stayed visible. The mark shrank back to the size it is on every
other page.

## What the audit found

All four directions were put through the UX checklist before they went
anywhere, and two findings came out of it. Both were in the sheet rather
than in a drawing, so both are fixed there and every surface gets them.

**No focus ring anywhere.** The sheet defined no `:focus-visible` at all,
and the one rule that touched focus removed it outright, so a keyboard
user could not see where they were. It now carries one accent ring with an
offset on every control it can reach. The composer's field is the one
control that draws none of its own: the box around it carries the accent
along its bottom edge, and that band is the whole of the mark, so a
keyboard user still sees where they are. Checked by tabbing through a real
page rather than by reading the rule: a row's link draws a 2px accent
outline at a 2px offset.

**No minimum touch target.** The by-width checks measured the layout and
never the reach, and the smallest controls were 20 to 28px tall against
the 44 a finger needs. Every form control now takes 44px at the phone
breakpoint, where before nothing did.

The contrast pair was measured rather than trusted to the token's name,
and it found a third: **`--dim` carried 12.5 and 13.5px labels in about
fifty places while measuring 3.51:1 on the page and 3.03:1 on a raised
card**, under the 4.5:1 that normal text needs. The uses were not wrong,
the value was, so the value changed: `#788294`, which is the **darkest**
step that clears AA on every ground the sheet puts text on, so the change
is the smallest one available rather than a comfortable one.

**Measured from the rendered page rather than from the two hex values**,
because the ground is a composite: the body carries a radial gradient over
`--bg` and a card carries one over `--s1`. Sampling the painted pixels and
reading the text colour the browser paints with:

| Pair, as rendered | Ratio |
|---|---|
| `--dim` on the page | 5.26:1 |
| `--dim` at the gradient's edge | 5.25:1 |
| `--dim` on a card | 4.98:1 |
| `--dim` at a card's deepest | 4.87:1 |
| `--muted` on the page | 7.01:1 |
| `--text` on a card | 12.23:1 |

**And looked at, because the number is not the whole test.** The three text
tokens still rank as a ranking: text, then muted, then dim, at both widths
on the home and on the connect screen. The gap between muted and dim is
narrower than it was - which is what passing costs - and there is no darker
step available, because a darker one fails on the raised card. The two
constraints meet at this value.

The keyboard and touch checks were clean on all four directions once the
ring and the target size were in, and every direction used the theme's own
tokens, so none of them needed a palette of its own.
