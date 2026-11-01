/**
 * The controls whose boundary is the whole of what says they are controls.
 *
 * **The defect behind this file.** A control whose fill is one step off its
 * ground, and whose only mark is a 1px `--line` boundary, is not visibly a
 * control at all: `--line` is the hairline (1.42:1 on the page), and WCAG
 * 1.4.11 asks 3:1 of a control's boundary. `--ctl` is the token for exactly
 * this job and already measures 3.50:1 on the page, 3.25:1 on `--s1` and
 * 3.02:1 on `--s2`; `contrast.test.ts` carries those pairs, and the mark
 * lifted 90/10 toward `--text` for the `--s3`-filled tab bar controls, where
 * plain `--ctl` measures 2.77:1.
 *
 * **Why a rule-level pin as well as the table.** That file measures tokens
 * against each other, and cannot see WHICH token a rule names - the half that
 * regressed on every site here. `1px solid var(--line)` is the sheet's most
 * common border, so a restyle that copies the rule beside it puts the
 * hairline back and leaves the palette green. `connect/controls.test.ts` pins
 * the connect screen the same way.
 *
 * **What the pin reads.** A rule's boundary is its `border`, or - for a field
 * that draws its edge as an inset shadow - its `box-shadow`, which is the
 * shape `.b-in` and `.ext-add-in` use. The whole value is compared, so a token
 * swapped for another cannot pass by naming something the table pairs.
 *
 * **A rule inside a media query is read too.** Two of the controls only get
 * their boundary at a narrow width, and a scanner that skipped every nested
 * rule could never fail for them.
 *
 * **The class this file closes is the `--line` one.** Every control whose
 * boxed boundary was the hairline now draws `--ctl`, or the `--s3` lift.
 *
 * **Named exclusions, so the class is a claim with edges.**
 * - `.files-filter` and `.sg-open` draw a bottom rule on a text field and a
 *   text link - the underline is the idiom, not a boxed control boundary - so
 *   they stay the hairline.
 * - `.qcard.cur`'s ring is recorded in `contrast.test.ts` as decoration.
 * - **Tone-tinted state borders are a separate class, and are NOT closed
 *   here**: `.sendb` (accent 45%, ~2.4:1), `.b-door` (~2.26), `.b-no` (~2.0),
 *   `.dock .btn.d` (~2.0), `.rv-a.danger` (~2.0) and
 *   `.settings-row-action.danger` (~1.8) draw a state tone as their boundary
 *   at rest. The tone carries the meaning, so whether each also needs a mark
 *   that holds 3:1 is its own decision; and because this pin reads a base
 *   selector, a state override is outside its reach by construction.
 */

import { readFileSync } from 'node:fs';

import { describe, expect, it } from 'vitest';

/**
 * Every `selector { declarations }` whose selector is `selector`, at any
 * depth, with comments stripped: a rule nested in an at-rule is a control's
 * own rule as much as a top-level one.
 */
function rulesIn(source: string, selector: string): string[] {
  const code = source.replace(/\/\*[\s\S]*?\*\//g, '');
  const found: string[] = [];
  let start = 0;
  for (let at = 0; at < code.length; at += 1) {
    const ch = code[at];
    if (ch === '{') {
      const head = code.slice(start, at).trim().replace(/\s+/g, ' ');
      if (head.startsWith('@')) {
        start = at + 1;
        continue;
      }
      const close = code.indexOf('}', at);
      if (close === -1) break;
      if (head === selector) found.push(code.slice(at + 1, close));
      at = close;
      start = close + 1;
    } else if (ch === '}') {
      start = at + 1;
    }
  }
  return found;
}

/** The value one rule body draws its boundary with, or null for a body that names none. */
function boundaryIn(body: string): string | null {
  const border = /border(?:-color)?:\s*([^;]+);/.exec(body);
  const borderValue = border?.[1]?.trim() ?? '';
  if (borderValue.includes('var(')) return borderValue;
  const shadow = /box-shadow:\s*([^;]+);/.exec(body);
  return shadow?.[1]?.trim() ?? null;
}

/**
 * The value the selector's boundary is drawn with: the first matching rule
 * that names one, so a media-query rule that only sets a width is skipped.
 */
function boundaryOf(selector: string, source: string): string {
  const bodies = rulesIn(source, selector);
  expect(bodies.length, `${selector} is drawn in the sheet`).toBeGreaterThan(0);
  for (const body of bodies) {
    const boundary = boundaryIn(body);
    if (boundary !== null) return boundary;
  }
  return 'a boundary naming no token';
}

const SHEET = readFileSync(new URL('./web.css', import.meta.url), 'utf8');
// The component's own style block: its markup's braces are not CSS, and the
// scanner would read them as blocks.
const REVIEW =
  /<style>([\s\S]*?)<\/style>/.exec(
    readFileSync(new URL('../review/Review.svelte', import.meta.url), 'utf8'),
  )?.[1] ?? '';

const CONTROL = '1px solid var(--ctl)';
const INSET = 'inset 0 0 0 1px var(--ctl)';
/** The `--s3`-filled controls: `--ctl` alone measures 2.77 there. */
const LIFTED = '1px solid color-mix(in srgb, var(--ctl) 90%, var(--text))';

describe('the controls that draw a boundary nothing else carries', () => {
  /**
   * One assertion per rule, because one can be restyled without the others:
   * the fields, the drawer's buttons, the dock's buttons and glyph buttons,
   * the copy button and its handoff twin, the two page searches, the tab
   * bar's pick and buttons, a fleet link, a retry, a take, and the header's
   * way back.
   */
  it('draws the sheet controls in the control mark rather than the hairline', () => {
    expect(boundaryOf('.b-in', SHEET), 'the board field').toBe(INSET);
    expect(boundaryOf('.dr-ask input', SHEET), "the drawer's answer field").toBe(CONTROL);
    expect(boundaryOf('.dr-btn', SHEET), "the drawer's buttons").toBe(CONTROL);
    expect(boundaryOf('.dock .btn', SHEET), "the dock's permission buttons").toBe(CONTROL);
    expect(boundaryOf('.micb', SHEET), "the dock's mic").toBe(CONTROL);
    expect(boundaryOf('.cp', SHEET), 'the copy button').toBe(CONTROL);
    expect(boundaryOf('.handoff .sessions .cp', SHEET), "the handoff row's copy").toBe(CONTROL);
    expect(boundaryOf('.extensions-page .ext-add-in', SHEET), 'the extensions add field').toBe(
      INSET,
    );
    expect(boundaryOf('.extensions-page .find', SHEET), "the extensions page's search").toBe(
      CONTROL,
    );
    expect(boundaryOf('.models-page .find', SHEET), "the models page's search").toBe(CONTROL);
    expect(boundaryOf('.tb-pick', SHEET), "the tab bar's pick").toBe(LIFTED);
    expect(boundaryOf('.tb-btn', SHEET), "the tab bar's buttons").toBe(LIFTED);
    expect(boundaryOf('.bz-list .bz-take', SHEET), "the browser's take button").toBe(CONTROL);
    expect(boundaryOf('.mine .retry', SHEET), "the refused echo's retry").toBe(CONTROL);
    expect(boundaryOf('.fleet-open', SHEET), "the fleet row's way to the board").toBe(CONTROL);
    expect(boundaryOf('header.top.pagetop .back', SHEET), "the page header's way back").toBe(
      CONTROL,
    );
    expect(boundaryOf('.tb-field', SHEET), "the takeover's field").toBe(CONTROL);
    expect(boundaryOf('.pop .dev', SHEET), "the device popover's row").toBe(CONTROL);
    expect(boundaryOf('.pop .chip', SHEET), "the dictation panel's chips").toBe(CONTROL);
    expect(boundaryOf('.rv-marker', SHEET), "the review thread's marker").toBe(CONTROL);
    expect(boundaryOf('.files-sheetclose', SHEET), "the files sheet's close, media only").toBe(
      CONTROL,
    );
    expect(
      boundaryOf('.settings-page .settings-rail-item', SHEET),
      'the settings rail item, media only',
    ).toBe(CONTROL);
    expect(boundaryOf('.follow', SHEET), 'the follow button').toBe(CONTROL);
    expect(boundaryOf('.needchip.calm', SHEET), "the session band's calm chip").toBe('var(--ctl)');
    expect(boundaryOf('.rv-a', SHEET), "the review's action button").toBe(CONTROL);
    expect(boundaryOf('.rv-txarea', SHEET), "the review's comment box").toBe(CONTROL);
    expect(boundaryOf('.files-sheetbtn', SHEET), "the files rail's sheet button").toBe(CONTROL);
    expect(boundaryOf('.extensions-page .btn.ghost', SHEET), 'the extensions ghost button').toBe(
      INSET,
    );
    expect(
      boundaryOf('.extensions-page .ext-scope', SHEET),
      "the extensions page's scope row",
    ).toBe(CONTROL);
    expect(boundaryOf('.extensions-page .ext-dlg-field select', SHEET), 'the install select').toBe(
      CONTROL,
    );
    expect(
      boundaryOf('.models-page button.chip, .extensions-page button.chip', SHEET),
      'a chip that is a button',
    ).toBe('var(--ctl)');
    // **A button's state keeps a boundary that holds.** The base rule is
    // outranked by each state's tint, and the warn tint measures 2.42:1, so
    // each state that is a BUTTON marks itself in its own solid tone.
    expect(
      boundaryOf('.extensions-page button.chip.warn', SHEET),
      'a warn chip that is a button',
    ).toBe('var(--warn)');
    expect(
      boundaryOf('.extensions-page button.chip.bad', SHEET),
      'a bad chip that is a button',
    ).toBe('var(--bad)');
    expect(
      boundaryOf('.extensions-page button.chip.blue', SHEET),
      'a blue chip that is a button',
    ).toBe('var(--blue)');
    expect(
      boundaryOf('.extensions-page button.chip.dim', SHEET),
      'a dim chip that is a button',
    ).toBe('var(--ctl)');
    expect(boundaryOf('.settings-page .settings-mode', SHEET), 'the settings edit door').toBe(
      CONTROL,
    );
    expect(
      boundaryOf('.settings-page .settings-add, .settings-page .settings-row-action', SHEET),
      "the settings page's row actions",
    ).toBe(CONTROL);
    expect(
      boundaryOf(
        '.settings-page .settings-setting input, .settings-page .settings-setting select, .list-editor input',
        SHEET,
      ),
      'the settings fields',
    ).toBe(CONTROL);
    expect(boundaryOf('.settings-btn.ghost', SHEET), 'the settings ghost button').toBe(INSET);
    expect(boundaryOf('.toggle .track', SHEET), 'the toggle track').toBe(CONTROL);
    expect(boundaryOf('.list-x', SHEET), 'the list remove button').toBe(CONTROL);
    expect(boundaryOf('.list-add', SHEET), 'the list add button').toBe('1px dashed var(--ctl)');
    expect(boundaryOf('.doc-page .doc-compose textarea', SHEET), 'the document comment box').toBe(
      CONTROL,
    );
    expect(
      boundaryOf(
        '.doc-page .doc-comment-btn, .doc-page .doc-cancel-btn, .doc-page .doc-finish-btn',
        SHEET,
      ),
      "the document page's actions",
    ).toBe(CONTROL);
  });

  it('draws the review controls in the control mark rather than the hairline', () => {
    expect(boundaryOf('.rv-scope-btn', REVIEW), 'the scope control').toBe(CONTROL);
    expect(boundaryOf('.rv-step-btn', REVIEW), 'the comment stepper').toBe(CONTROL);
    expect(
      boundaryOf('.rv-cstep-n', REVIEW),
      "the stepper's count cell shares its arrows' boundary",
    ).toBe(CONTROL);
    expect(boundaryOf('.rv-mode', REVIEW), 'the view-mode switch').toBe(CONTROL);
  });
});
