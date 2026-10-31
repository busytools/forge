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
 */

import { readFileSync } from 'node:fs';

import { describe, expect, it } from 'vitest';

/**
 * One top-level `selector { declarations }`, with comments stripped.
 *
 * A rule nested inside an at-rule is skipped rather than read: a `@media`
 * body holds its own rules, and several of the controls here have a coarse
 * pointer rule that sets a width and names no boundary.
 */
function ruleIn(source: string, selector: string): string | null {
  const code = source.replace(/\/\*[\s\S]*?\*\//g, '');
  let depth = 0;
  let start = 0;
  for (let at = 0; at < code.length; at += 1) {
    if (code[at] === '{') {
      const head = code.slice(start, at).trim().replace(/\s+/g, ' ');
      if (depth === 0 && !head.startsWith('@')) {
        const close = code.indexOf('}', at);
        if (close === -1) break;
        if (head === selector) return code.slice(at + 1, close);
        at = close;
        start = close + 1;
        continue;
      }
      depth += 1;
      start = at + 1;
    } else if (code[at] === '}') {
      depth -= 1;
      start = at + 1;
    }
  }
  return null;
}

/** The value the rule draws its boundary with: its border, else its inset shadow. */
function boundaryOf(selector: string, source: string): string {
  const body = ruleIn(source, selector);
  expect(body, `${selector} is drawn in the sheet`).not.toBeNull();
  const border = /border(?:-color)?:\s*([^;]+);/.exec(body ?? '');
  const borderValue = border?.[1]?.trim() ?? '';
  if (borderValue.includes('var(')) return borderValue;
  const shadow = /box-shadow:\s*([^;]+);/.exec(body ?? '');
  return shadow?.[1]?.trim() ?? 'a boundary naming no token';
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
    expect(boundaryOf('.settings-page button.settings-mode', SHEET), 'the settings mode pill').toBe(
      CONTROL,
    );
    expect(
      boundaryOf('.settings-page .settings-add, .settings-page .settings-row-action', SHEET),
      "the settings page's row actions",
    ).toBe(CONTROL);
    expect(
      boundaryOf(
        '.settings-page .settings-rows input, .settings-page .settings-rows select, .list-editor input',
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
