/**
 * The connect screen's two controls, and the token that identifies them.
 *
 * **The defect behind this file.** The address field and the Connect button
 * drew their boundary with the hairline token, `--line` (1.42:1 on the page),
 * over fills one step off the page (`--s1` at 1.08:1, `--s2` at 1.16:1).
 * Nothing separated either control from the page, so the border was the whole
 * of what said a control was there - under the 3:1 WCAG 1.4.11 asks of a
 * control's boundary. These two draw `--ctl` now: 3.25:1 against `--s1` and
 * 3.02:1 against `--s2`. That is a claim about this screen only - other
 * controls still carry `--line` over a ground that does not separate them,
 * and #2306 is the sweep for those.
 *
 * **Why the pin is here rather than only in `contrast.test.ts`.** That file
 * measures the palette against itself, and it now carries the pairs this
 * screen's grounds need - but it cannot see WHICH token a rule names, which is
 * the half that regressed. `1px solid var(--line)` is the tree's most common
 * border, so a restyle that copies the rule beside it puts the hairline back
 * and leaves the suite green. The Composer's box is pinned at the rule level
 * the same way.
 *
 * **A hole, named rather than left to be discovered:** a rule nested inside an
 * at-rule is not read here, and this component has one `@media` block. It
 * draws a type size and no boundary, and a control re-drawn inside a media
 * query would leave this file's scope rather than fail it.
 */

import { readFileSync } from 'node:fs';

import { describe, expect, it } from 'vitest';

const CONNECT = readFileSync(new URL('./Connect.svelte', import.meta.url), 'utf8');

/** Every `selector { declarations }` in the component's own style block. */
function rules(): { selector: string; body: string }[] {
  const style = /<style>([\s\S]*?)<\/style>/.exec(CONNECT)?.[1] ?? '';
  const code = style.replace(/\/\*[\s\S]*?\*\//g, '');
  const found: { selector: string; body: string }[] = [];
  for (const held of code.split('}')) {
    const at = held.indexOf('{');
    if (at === -1) continue;
    const selector = held.slice(0, at).trim().replace(/\s+/g, ' ');
    // An at-rule's head opens a block rather than a rule, and this walk stops
    // at the first `{` - so the nested rule is skipped with it.
    if (selector === '' || selector.startsWith('@')) continue;
    found.push({ selector, body: held.slice(at + 1) });
  }
  return found;
}

/** The colour token the rule whose selector contains `fragment` borders with. */
function borderToken(fragment: string): string {
  const rule = rules().find(({ selector }) => selector.includes(fragment));
  expect(rule, `Connect.svelte draws no rule for ${fragment}`).toBeDefined();
  const token = /border:[^;]*var\((--[\w-]+)\)/.exec(rule?.body ?? '')?.[1];
  return token ?? 'a border naming no token';
}

describe("the connect screen's controls", () => {
  /**
   * One assertion per control, because one can be restyled without the other:
   * the field is a global rule reaching the shared `Field`, and the button is
   * this component's own element.
   */
  it('draws both controls in the control mark rather than the hairline', () => {
    expect(borderToken('.field :global(input)'), 'the address field').toBe('--ctl');
    expect(borderToken('button'), 'the connect button').toBe('--ctl');
  });
});
