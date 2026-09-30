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

  it('carries the toggle word as text rather than as a stylesheet rule', () => {
    // A `::after` label is generated content, so the disclosure's accessible
    // name is whatever the user agent makes of it - and this row is the whole
    // of what a reader who cannot see the chip is told.
    expect(draw(1)).toContain('>expand<');
  });

  it('draws the hooks behind the count, in the unit their durations are in', () => {
    expect(draw(1)).toContain('echo fixture-stop-hook-ok');
    expect(draw(1), 'the duration the captured row records').toContain('3ms');
    expect(draw(1, [{ command: 'cargo fmt --check', durationMs: 1180 }])).toContain('1.2s');
  });
});
