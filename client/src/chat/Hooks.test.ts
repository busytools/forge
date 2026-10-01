import { render } from 'svelte/server';
import { describe, expect, it } from 'vitest';

import Hooks from './Hooks.svelte';
import type { HookInfo } from './units';

const ONE: HookInfo[] = [{ command: 'echo fixture-stop-hook-ok', durationMs: 3 }];

const draw = (actions: number, infos: HookInfo[] = ONE): string =>
  render(Hooks, { props: { actions, infos } }).body;

describe('the hook chip a turn carries', () => {
  it('counts one hook in the singular', () => {
    expect(draw(1), 'the count the captured row carries').toContain('1 action<');
    expect(draw(2)).toContain('2 actions<');
  });

  it('carries the disclosure chevron the rest of the column draws, and no spelled-out verb', () => {
    // Every other `<details>` here leads its toggle with the shared chevron,
    // so this chip spelling `expand` instead is a second convention for one
    // job. The chip keeps its own words either way, which is what a reader
    // who cannot see the glyph is told the disclosure is.
    const body = draw(1);

    // One chevron, and the one the stylesheet's open/closed rules reach. The
    // chip used to lead with a chevron that meant nothing by its state, so a
    // second one beside it would be two answers to the same question.
    expect(body.match(/#i-chev/g), 'exactly one disclosure chevron').toHaveLength(1);
    expect(body, 'and it is the shared one the stylesheet turns').toContain('class="ic arw"');
    expect(body, 'and not the verb spelled out in its place').not.toMatch(
      />\s*(expand|collapse)\s*</,
    );
  });

  it('draws the hooks behind the count, in the unit their durations are in', () => {
    expect(draw(1)).toContain('echo fixture-stop-hook-ok');
    expect(draw(1), 'the duration the captured row records').toContain('3ms');
    expect(draw(1, [{ command: 'cargo fmt --check', durationMs: 1180 }])).toContain('1.2s');
  });
});
