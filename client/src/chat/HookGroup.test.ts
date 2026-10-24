import { readFileSync } from 'node:fs';

import { render } from 'svelte/server';
import { describe, expect, it } from 'vitest';

import HookGroup from './HookGroup.svelte';
import { repoPath } from '../repo';
import type { HookGroupLeaf, HookLeaf } from './units';

/** One run of a session start, its output empty where `body` is null. */
const run = (n: number, body: string | null = null, failed = false): HookLeaf => ({
  key: `hook-${n}`,
  run: { name: 'SessionStart:startup', event: null, failed, body },
});

/** A start carrying `runs`, keyed by the first as the fold keys it. */
const start = (runs: HookLeaf[]): HookGroupLeaf => ({
  key: runs[0]?.key ?? 'hook-0',
  event: 'SessionStart',
  runs,
});

/**
 * Eleven hooks, two of them with output: the shape a fresh session opens on,
 * and the count Ved named when he asked for the row.
 */
const ELEVEN = start([
  run(0, 'repository is clean\n'),
  run(1, 'the PR review gate\n'),
  ...Array.from({ length: 9 }, (_, n) => run(n + 2)),
]);

const draw = (group: HookGroupLeaf, open = false): string =>
  render(HookGroup, { props: { group, open } }).body;

const summaryOf = (body: string): string =>
  body.slice(body.indexOf('<summary'), body.indexOf('</summary>'));

const SHEET = readFileSync(new URL('../assets/web.css', import.meta.url), 'utf8');
const PAGE = readFileSync(repoPath('docs/book/src/ui/client/web-session.html'), 'utf8');
const BOOK = /<style>([\s\S]*?)<\/style>/.exec(PAGE)?.[1] ?? '';

describe("a session start's row", () => {
  it('says what fired and how many hooks it carried, and draws nothing else closed', () => {
    const closed = draw(ELEVEN);
    const summary = summaryOf(closed);

    expect(summary, 'the event in words and the count it carried').toContain(
      'session start \u{b7} 11 hooks',
    );
    expect(summary, 'the shared disclosure chevron').toContain('class="ic arw"');
    expect(summary, 'the hook family mark every hook row leads with').toContain('#i-hook');
    expect(
      closed.slice(closed.indexOf('</summary>')),
      'and none of the hooks under a closed row',
    ).not.toContain('SessionStart:startup');
  });

  it('counts one hook in the singular', () => {
    expect(draw(start([run(0, 'said\n')]))).toContain('session start \u{b7} 1 hook<');
  });

  it('opens onto each hook that said something, and draws no row for the silent ones', () => {
    const open = draw(ELEVEN, true);

    expect(open, 'a row for each hook with output').toContain('repository is clean');
    expect(open, 'and the other one').toContain('the PR review gate');
    expect(
      open.match(/class="leaf hookrow"/g),
      'and nothing for the nine that printed none',
    ).toHaveLength(2);
    expect(open, 'while the count still says how many the firing carried').toContain('11 hooks');
  });

  it('keeps a failed hook that printed nothing, and marks the closed row', () => {
    const failed = start([run(0, 'said\n'), run(1, null, true)]);

    expect(summaryOf(draw(failed)), 'the failure mark on the closed row').toContain(
      'class="ic gl err"',
    );
    expect(summaryOf(draw(failed)), 'with no claim of failure beyond it').not.toContain('#i-x');
    expect(
      draw(failed, true).match(/class="leaf hookrow"/g),
      'and the silent failure stays as a row of its own',
    ).toHaveLength(2);

    expect(
      summaryOf(draw(start([run(0, 'said\n')]))),
      'a clean start carries no failure mark',
    ).toContain('class="ic gl ok"');
  });

  it('draws the row in both sheets, and the book draws it closed and open', () => {
    // The group is a leaf row like any other, so it takes the shared row
    // styling; what each sheet owes it is its own box rule and the inset the
    // hooks sit in, or the page and the app draw two different rows.
    for (const [where, sheet] of [
      ['web.css', SHEET],
      ['the book drawing', BOOK],
    ] as const) {
      expect(sheet, `${where} spells the group's own box`).toMatch(
        /details\.hookgroup|\.hookgroup/,
      );
      expect(sheet, `${where} indents the hooks under it`).toMatch(
        /\.hookgroup\s*>\s*\.body\s*\{[^}]*border-left/,
      );
    }

    const closed = /<details class="leaf hookgroup">[\s\S]*?<\/details>/.exec(PAGE)?.[0] ?? '';
    expect(closed, 'the book draws a closed start').not.toBe('');
    expect(closed, 'naming the event and its count').toMatch(/session start &#183; \d+ hooks/);

    const open = /<details class="leaf hookgroup" open>[\s\S]*?<\/details>/.exec(PAGE)?.[0] ?? '';
    expect(open, 'and an open one').not.toBe('');
    expect(open, 'holding the hook rows it opens onto').toContain('class="leaf hookrow"');
  });
});
