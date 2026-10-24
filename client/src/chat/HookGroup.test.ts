import { readFileSync } from 'node:fs';

import { render } from 'svelte/server';
import { describe, expect, it } from 'vitest';

import HookGroup from './HookGroup.svelte';
import { repoPath } from '../repo';
import type { HookGroupLeaf, HookLeaf } from './units';

/** One run of a session start, its output empty where `body` is null. */
const run = (n: number, body: string | null = null, failed = false, settled = true): HookLeaf => ({
  key: `hook-${n}`,
  run: { name: 'SessionStart:startup', event: null, failed, settled, body },
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

  it('keeps a failed hook that printed nothing, and says so in words on the closed row', () => {
    const failed = start([run(0, 'said\n'), run(1, null, true)]);
    const summary = summaryOf(draw(failed));

    // Rule 22: the tone is one carrier, not the only one. A reader who cannot
    // see the glyph reads the count in the label.
    expect(summary, 'the failure mark on the closed row').toContain('class="ic gl err"');
    expect(summary, 'and the failure in words beside the count').toContain(
      'session start \u{b7} 2 hooks \u{b7} 1 failed',
    );
    expect(summary, 'with no claim of failure beyond it').not.toContain('#i-x');
    expect(
      draw(failed, true).match(/class="leaf hookrow"/g),
      'and the silent failure stays as a row of its own',
    ).toHaveLength(2);

    const clean = summaryOf(draw(start([run(0, 'said\n')])));
    expect(clean, 'a clean start carries the passing tone').toContain('class="ic gl ok"');
    expect(clean, 'and no failure in its words either').not.toContain('failed');
  });

  it('keeps a hook that has not settled, and says why the open holds nothing', () => {
    // A run that has only started is still out: its name is the whole of what
    // the reader can know, and hiding it would draw the firing as settled.
    const running = start([run(0, null, false, false), run(1, 'said\n')]);
    expect(
      draw(running, true).match(/class="leaf hookrow"/g),
      'the running hook draws by its name beside the one that spoke',
    ).toHaveLength(2);
    expect(
      summaryOf(draw(running)),
      'and the start carries no passing tone while a hook is still out',
    ).toMatch(/class="ic gl"/);

    // Every hook finished without a word: the row still opens, and says why it
    // holds none rather than opening onto nothing.
    const quiet = draw(start([run(0), run(1)]), true);
    expect(quiet, 'a muted line in place of the rows').toContain('the hooks printed nothing');
    expect(quiet.match(/class="leaf hookrow"/g), 'and no empty hook rows').toBeNull();
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

    const drawn = starts(PAGE);
    // The denominator: an extraction that read no start would report every
    // shape missing, and one that stopped at a nested row would say nothing
    // about what the start itself holds.
    expect(drawn.length, 'the book draws every state of the start').toBeGreaterThanOrEqual(4);

    const closed = drawn.find((start) => !start.open && !start.html.includes('gl err'))?.html ?? '';
    expect(closed, 'the book draws a closed start').not.toBe('');
    expect(closed, 'naming the event and its count').toMatch(/session start &#183; \d+ hooks/);

    const open = drawn.find((start) => start.open && !start.html.includes('gl err'))?.html ?? '';
    expect(open, 'and an open one').not.toBe('');
    expect(open, 'holding the hook rows it opens onto').toContain('class="leaf hookrow"');
    expect(open, 'and not the line that stands in for hooks').not.toContain(
      'the hooks printed nothing',
    );

    // The failed state, which the component carries for a silent hook: a
    // closed start naming the failure in words as well as tone, and an open
    // one keeping the silent failed hook as a row whose summary has no tail.
    const failedShut = drawn.find((start) => !start.open && start.html.includes('gl err'))?.html;
    expect(failedShut ?? '', 'the book draws a failed start, marked while closed').not.toBe('');
    expect(failedShut ?? '', 'with the failure in words, not the tone alone').toMatch(
      /session start &#183; \d+ hooks &#183; \d+ failed/,
    );

    const failedOpen = drawn.find((start) => start.open && start.html.includes('gl err'))?.html;
    expect(failedOpen ?? '', 'and opens one onto the failure it kept').not.toBe('');
    expect(failedOpen ?? '', 'holding the silent failed hook as a row with no tail slot').toMatch(
      /<svg class="ic gl err"><use href="#i-hook"\/><\/svg>\s*<span class="tn">notify\.sh \(SessionStart\)<\/span>\s*<svg class="ic arw">/,
    );

    // And the all-silent state: the row opens onto a muted line rather than
    // onto nothing, and holds no hook row.
    const quiet =
      drawn.find((start) => start.open && start.html.includes('the hooks printed nothing'))?.html ??
      '';
    expect(quiet, 'the book draws the start whose hooks all finished silent').not.toBe('');
    expect(quiet, 'with no hook rows to list').not.toContain('class="leaf hookrow"');

    // And the start still out: neutral until every hook has answered.
    expect(
      drawn.some((start) => start.html.includes('<svg class="ic gl">')),
      'the book draws a start whose hook has not settled, with no passing tone',
    ).toBe(true);
  });
});

/**
 * A `<details>` block from its opener to its own close, nested rows included.
 *
 * **The non-greedy regex a drawing test reaches for first stops at the first
 * nested row's close**, so an open start would be read as one hook and the
 * rest of its body would go unchecked. Counting the nested openers is what
 * makes the group's own body the thing that is read.
 */
function detailsFrom(page: string, from: number): string {
  let depth = 0;
  let at = from;
  while (at < page.length) {
    const open = page.indexOf('<details', at);
    const close = page.indexOf('</details>', at);
    if (close === -1) return '';
    if (open !== -1 && open < close) {
      depth += 1;
      at = open + '<details'.length;
      continue;
    }
    depth -= 1;
    at = close + '</details>'.length;
    if (depth === 0) return page.slice(from, at);
  }
  return '';
}

/** Every session-start row the drawing holds, with its own state. */
function starts(page: string): { open: boolean; html: string }[] {
  return [...page.matchAll(/<details class="leaf hookgroup"( open)?>/g)].map((match) => ({
    open: match[1] !== undefined,
    html: detailsFrom(page, match.index ?? 0),
  }));
}
