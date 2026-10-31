/**
 * The controls whose boundary is the whole of what says they are controls.
 *
 * **The defect behind this file.** A control whose fill is one step off its
 * ground, and whose only mark is a 1px `--line` boundary, is not visibly a
 * control at all: `--line` is the hairline (1.42:1 on the page), and WCAG
 * 1.4.11 asks 3:1 of a control's boundary. `--ctl` is the token for exactly
 * this job and already measures 3.50:1 on the page, 3.25:1 on `--s1` and
 * 3.02:1 on `--s2`; `contrast.test.ts` carries those pairs.
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
 * shape `.b-in` and `.ext-add-in` use. A rule that names neither is reported
 * rather than passed.
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

/** The colour token the rule draws its boundary with. */
function boundaryToken(selector: string, source: string): string {
  const body = ruleIn(source, selector);
  expect(body, `${selector} is drawn in the sheet`).not.toBeNull();
  const border = /border(?:-color)?:[^;]*var\((--[\w-]+)\)/.exec(body ?? '');
  if (border) return border[1] ?? 'a border naming no token';
  const shadow = /box-shadow:[^;]*var\((--[\w-]+)\)/.exec(body ?? '');
  return shadow?.[1] ?? 'a boundary naming no token';
}

const SHEET = readFileSync(new URL('./web.css', import.meta.url), 'utf8');
// The component's own style block: its markup's braces are not CSS, and the
// scanner would read them as blocks.
const REVIEW =
  /<style>([\s\S]*?)<\/style>/.exec(
    readFileSync(new URL('../review/Review.svelte', import.meta.url), 'utf8'),
  )?.[1] ?? '';

describe('the controls that draw a boundary nothing else carries', () => {
  /**
   * One assertion per rule, because one can be restyled without the others:
   * the fields, the drawer's buttons, the dock's buttons and glyph buttons,
   * the copy button and its handoff twin, and the extensions page's add
   * field.
   */
  it('draws the sheet controls in the control mark rather than the hairline', () => {
    expect(boundaryToken('.b-in', SHEET), 'the board field').toBe('--ctl');
    expect(boundaryToken('.dr-ask input', SHEET), "the drawer's answer field").toBe('--ctl');
    expect(boundaryToken('.dr-btn', SHEET), "the drawer's buttons").toBe('--ctl');
    expect(boundaryToken('.dock .btn', SHEET), "the dock's permission buttons").toBe('--ctl');
    expect(boundaryToken('.micb', SHEET), "the dock's mic").toBe('--ctl');
    expect(boundaryToken('.cp', SHEET), 'the copy button').toBe('--ctl');
    expect(boundaryToken('.handoff .sessions .cp', SHEET), "the handoff row's copy").toBe('--ctl');
    expect(boundaryToken('.extensions-page .ext-add-in', SHEET), 'the extensions add field').toBe(
      '--ctl',
    );
  });

  it('draws the review controls in the control mark rather than the hairline', () => {
    expect(boundaryToken('.rv-scope-btn', REVIEW), 'the scope control').toBe('--ctl');
    expect(boundaryToken('.rv-step-btn', REVIEW), 'the comment stepper').toBe('--ctl');
    expect(
      boundaryToken('.rv-cstep-n', REVIEW),
      "the stepper's count cell shares its arrows' boundary",
    ).toBe('--ctl');
  });
});
