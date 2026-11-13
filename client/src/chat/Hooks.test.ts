import { readFileSync } from 'node:fs';

import { render } from 'svelte/server';
import { describe, expect, it } from 'vitest';

import { repoPath } from '../repo';
import Hooks from './Hooks.svelte';
import type { HookBatchLeaf, HookInfo } from './units';

const ONE: HookInfo[] = [{ command: 'echo fixture-stop-hook-ok', durationMs: 3 }];

/** The batch the fold draws from a `stop_hook_summary` frame. */
const batch = (actions: number, infos = ONE, errors: string[] = []): HookBatchLeaf => ({
  key: 'uuid_7#hookbatch',
  actions,
  infos,
  errors,
});

/** The row drawn open, which is the state its body needs. */
const draw = (batchLeaf: HookBatchLeaf, open = true): string =>
  render(Hooks, { props: { batch: batchLeaf, open } }).body;

/**
 * The row's own `<summary>`. The closed state has to live here: a mark or a
 * count drawn anywhere inside the open body only is a state a reader sees after
 * opening the row, which is the blind spot the failed state exists to close.
 */
const summaryOf = (body: string): string =>
  body.slice(body.indexOf('<summary'), body.indexOf('</summary>'));

const PAGE = readFileSync(repoPath('docs/book/src/ui/client/web-session.html'), 'utf8');

/**
 * Every `<div class="leaves">` block the drawing holds, nested divs included.
 *
 * **The batch is a tool row, so where it sits is part of the shape**: a row
 * drawn directly under the turn is the line this cut moved, and a substring
 * match anywhere on the page would not tell the two apart.
 */
function leavesBlocks(page: string): string[] {
  const out: string[] = [];
  for (const match of page.matchAll(/<div class="leaves">/g)) {
    const from = match.index ?? 0;
    let depth = 0;
    let at = from;
    while (at < page.length) {
      const open = page.indexOf('<div', at);
      const close = page.indexOf('</div>', at);
      if (close === -1) break;
      if (open !== -1 && open < close) {
        depth += 1;
        at = open + '<div'.length;
        continue;
      }
      depth -= 1;
      at = close + '</div>'.length;
      if (depth === 0) {
        out.push(page.slice(from, at));
        break;
      }
    }
  }
  return out;
}

/** The batch rows the tool lists draw, from their opener to their own close. */
const batches = (): string[] =>
  leavesBlocks(PAGE).flatMap(
    (block) => block.match(/<details class="leaf hookgroup hookbatch"[\s\S]*?<\/details>/g) ?? [],
  );

describe('the hook batch a turn carries', () => {
  /**
   * A closed row carries its summary and nothing else: the commands are the
   * body's, and the giant seat's cost is the bodies of rows nobody opened.
   */
  it('carries summary markup only while it is closed', () => {
    // No `open` prop: the production default, which is what a reader meets.
    const closed = render(Hooks, { props: { batch: batch(1) } }).body;
    const under = closed.slice(closed.indexOf('</summary>'));
    expect(under, 'no body under a closed row').not.toContain('echo fixture-stop-hook-ok');

    const open = draw(batch(1));
    expect(open, 'and the commands are drawn onto the open').toContain('echo fixture-stop-hook-ok');
  });

  it('counts one hook in the singular', () => {
    expect(draw(batch(1)), 'the count the captured row carries').toContain('1 action<');
    expect(draw(batch(2))).toContain('2 actions<');
  });

  /**
   * The row is the tool row every call draws: the kind's glyph, the title, the
   * disclosure chevron - not a second system inside the group. A settled batch
   * takes the passing tone its own batch of hooks earned.
   */
  it('draws as a tool row, with the hook glyph and the shared chevron', () => {
    const body = draw(batch(1));

    expect(body, 'the leaf row the run draws as').toContain(
      '<details class="leaf hookgroup hookbatch"',
    );
    const summary = summaryOf(body);
    expect(summary, 'the hook family mark every hook row leads with').toContain('#i-hook');
    expect(summary, 'a settled batch takes the passing tone').toContain('class="ic gl ok"');
    // One chevron, and the one the stylesheet's open/closed rules reach.
    expect(body.match(/#i-chev/g), 'exactly one disclosure chevron').toHaveLength(1);
    expect(summary, 'and it is the shared one the stylesheet turns').toContain('class="ic arw"');
    expect(summary, 'the title the leaf rules clamp').toContain('<span class="tn">');
    expect(body, 'and not the verb spelled out in its place').not.toMatch(
      />\s*(expand|collapse)\s*</,
    );
  });

  /**
   * A state visible only when the row is open is where the chip's last defect
   * lived, so the failure has to reach the closed row: the hook glyph takes the
   * bad tone, and the count of what the CLI reported rides the label.
   */
  it('marks the row failed, and counts the errors, when the CLI reported any', () => {
    const failed = summaryOf(draw(batch(1, ONE, ['JSON validation failed'])));

    expect(failed, 'the failure tone on the hook glyph').toContain('class="ic gl err"');
    expect(failed, 'with the count beside the action count').toContain('1 error<');
    expect(
      summaryOf(draw(batch(1, ONE, ['one', 'two']))),
      'plural when more than one came back',
    ).toContain('2 errors<');

    const clean = summaryOf(draw(batch(1)));
    expect(clean, 'a clean batch keeps the passing tone').toContain('class="ic gl ok"');
    expect(clean, 'and says nothing about errors').not.toContain('error');
  });

  /**
   * The corpus' most common failing shape is more hooks than errors - the
   * errors name no hook - so the two draw as their own lists rather than an
   * error against a command.
   */
  it('draws each error as its own row under the hooks, in the bad tone', () => {
    const body = draw(
      batch(
        2,
        [
          { command: 'bash hooks/check.sh', durationMs: 6 },
          { command: 'cargo fmt --check', durationMs: 412 },
        ],
        ['JSON validation failed'],
      ),
    );

    expect(body, 'the error as the row itself, in the tone the sheets give a failure').toContain(
      '<div class="term"><span class="fail">JSON validation failed</span></div>',
    );
    expect(body, 'both hooks still drawn, so neither list replaces the other').toContain(
      'cargo fmt --check',
    );

    expect(
      draw(batch(1, [], ['JSON validation failed'])),
      'and the errors draw even where the rows they came from carry none',
    ).toContain('<span class="fail">JSON validation failed</span>');
  });

  it('draws the hooks behind the count, in the unit their durations are in', () => {
    expect(draw(batch(1))).toContain('echo fixture-stop-hook-ok');
    expect(draw(batch(1)), 'the duration the captured row records').toContain('3ms');
    expect(draw(batch(1, [{ command: 'cargo fmt --check', durationMs: 1180 }]))).toContain('1.2s');
  });

  /**
   * The book's page is the drawing the surface is judged by, and the row moved
   * into the tool list: a sample left under the turn as a line of its own would
   * draw the shape this cut removed.
   */
  it('draws the batch in the book as a tool row, with the glyph and the chevron', () => {
    const drawn = batches();
    expect(
      drawn.length,
      'the book draws the closed, open and failed shapes',
    ).toBeGreaterThanOrEqual(3);

    const closed = drawn.find((row) => !row.includes(' open>')) ?? '';
    expect(closed, 'the book draws the row closed').not.toBe('');
    const closedSummary = summaryOf(closed);
    expect(closedSummary, 'the hook glyph').toContain('#i-hook');
    expect(closedSummary, 'the shared chevron').toContain('class="ic arw"');
    expect(closedSummary, 'and the count it carries').toMatch(/hook summary &#183; \d+ actions?/);

    const open = drawn.find((row) => row.includes(' open>') && !row.includes('gl err'));
    expect(open ?? '', 'the book draws the row open, where its rows can be seen').not.toBe('');
    expect(open ?? '', 'with the rows the row holds').toContain('class="body"');
    const counted = /hook summary &#183; (\d+) actions?/.exec(open ?? '')?.[1] ?? '';
    expect(counted, 'the row says how many actions it took').not.toBe('');
    const rows = (open ?? '').match(/<div class="term">[^<]+<\/div>/g) ?? [];
    expect(rows.length, 'a row drawn per action the row counted').toBe(Number(counted));
    expect(
      rows.filter((row) => !/<div class="term">[^<]+ &#183; .*\d+(\.\d+)?(ms|s)/.test(row)),
      'every row a command with the duration it took',
    ).toEqual([]);
  });

  /**
   * The failed half the row exists for, and a page drawing only the passing row
   * is the same one-state blind spot the open drawing was added to close. What
   * holds is the SHAPE: the failure tone on the closed row and the count, so the
   * failure shows without opening it, and the error as a row under the hooks.
   */
  it('draws the failed batch in the book, its state on the closed summary', () => {
    const failed =
      batches().find(
        (row) =>
          row.includes('gl err') && /hook summary &#183; \d+ actions? &#183; \d+ errors?/.test(row),
      ) ?? '';
    expect(failed, 'the book draws the failed batch open').not.toBe('');

    const summary = summaryOf(failed);
    expect(summary, 'the failure tone on the hook glyph').toContain('class="ic gl err"');
    expect(summary, 'and the count beside the action count').toMatch(
      /hook summary &#183; \d+ actions? &#183; \d+ errors?/,
    );
    expect(failed, 'the error as its own row, in the tone the sheets give a failure').toContain(
      '<div class="term"><span class="fail">',
    );
  });
});
