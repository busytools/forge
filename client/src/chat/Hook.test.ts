import { readFileSync } from 'node:fs';

import { render } from 'svelte/server';
import { describe, expect, it } from 'vitest';

import Hook from './Hook.svelte';
import type { HookRun } from './units';

/**
 * The hook run row, as markup.
 *
 * **What this can answer and what it cannot.** The row is a disclosure, so its
 * accessible name is its summary and the thing to assert is text: a reader who
 * cannot see it has only those words to know a hook ran and how it ended. The
 * open body is what the fold sent, which is a fact about the markup. Where the
 * mark and the chevron land on the row is layout, which jsdom does not perform.
 */

const SHEET = readFileSync(new URL('../assets/web.css', import.meta.url), 'utf8');
const PAGE = readFileSync(
  new URL('../../../docs/book/src/ui/client/web-session.html', import.meta.url),
  'utf8',
);

/** The book page's own copy of the rules it draws with. */
const BOOK = /<style>([\s\S]*?)<\/style>/.exec(PAGE)?.[1] ?? '';

const run = (extra: Partial<HookRun> = {}): HookRun => ({
  name: 'SessionStart:startup',
  event: null,
  state: 'success \u{b7} exit 0',
  failed: false,
  body: 'capture-line-1\ncapture-line-2\n',
  ...extra,
});

const draw = (over: Partial<HookRun> = {}): string =>
  render(Hook, { props: { run: run(over) } }).body;

/** The closed row itself: the markup a mark or a chevron is read off. */
const summaryOf = (body: string): string =>
  body.slice(body.indexOf('<summary'), body.indexOf('</summary>'));

/** The closed row's own words, with Svelte's block markers and the tags off. */
const summaryWords = (body: string): string =>
  summaryOf(body)
    .replace(/<!--.*?-->/g, '')
    .replace(/<[^>]*>/g, '')
    .replace(/\s+/g, ' ')
    .trim();

describe('the hook run row', () => {
  it('names the hook and the state it reached on the closed row', () => {
    const body = draw();
    const said = summaryWords(body);

    expect(said, 'the kind, ahead of the name').toMatch(/^hook\b/);
    expect(said, 'the hook the CLI matched, under its own name').toContain('SessionStart:startup');
    expect(said, 'and how the run ended, without opening it').toContain('success \u{b7} exit 0');
    // Once, not twice: every captured name already carries its event, so a row
    // repeating it would say `SessionStart:startup (SessionStart)`.
    expect(said, 'and the event only where the name does not say it').not.toContain(
      '(SessionStart)',
    );
    const named = summaryWords(draw({ event: 'SessionStart', name: 'notify.sh' }));
    expect(named, 'a name that carries none keeps its own words').toContain('notify.sh');
    expect(named, 'and the event draws beside it').toContain('(SessionStart)');

    // One chevron, and the one the sheets' open/closed rules reach: a second
    // answer to "is this open" is what the chip's own defect was.
    expect(body.match(/#i-chev/g), 'exactly one disclosure chevron').toHaveLength(1);
    expect(body, 'and it is the shared one the sheets turn').toContain('class="ic arw"');
  });

  /**
   * The failed half, and where it has to land: a state drawn anywhere inside the
   * open body only is one a reader sees after opening the row, which is the
   * blind spot the chip's own failed state was moved out of.
   */
  it('marks a run that failed on the closed row', () => {
    const failed = summaryOf(draw({ failed: true }));
    expect(failed, 'the failure mark a failed call, turn or run leads with').toContain('#i-x');
    expect(failed, 'drawn as the shared state mark').toContain('class="ic st err"');
    expect(summaryOf(draw()), 'and a run that exited clean carries no mark').not.toContain('#i-x');

    for (const [what, sheet] of sheets()) {
      // The selector alone proves nothing, so the rule and the declaration are
      // pinned together: dropping the mark's class or swapping the token is what
      // turns a failed run green.
      expect(sheet, `${what} colours the failed row's mark`).toMatch(
        /details\.hookrun > summary \.st\.err \{[^}]*color: var\(--bad\)/,
      );
    }
  });

  /**
   * The whole of what the hook printed, which is the reason the row collapses
   * rather than clips: a summary standing in for the output would be the drop
   * rule 25 names, so the body has to carry all of it, however long.
   */
  it('holds the whole output behind the open, unclipped', () => {
    const long = Array.from({ length: 60 }, (_, at) => `line ${at}`).join('\n');
    expect(draw({ body: long }), 'the last line the hook printed').toContain('line 59');
    expect(draw(), 'and every line before it').toContain('capture-line-2');
  });

  it('carries no body for a run that printed nothing', () => {
    expect(draw({ body: null }), 'no empty box under a hook that said nothing').not.toContain(
      'class="body"',
    );
    expect(draw(), 'where one that printed carries it').toContain('class="body"');
  });

  /**
   * **A bare class name reaches further than the row it was written for.**
   * Drawn as `.kind`, the thinking row inherited the family-tree disclosure's
   * 3px margins - the trap the sheet's `.knd` and `.tkind` were both split out
   * of. So every class this row wears must have no bare rule of that name
   * anywhere either sheet reads, and the name of the row itself is one letter
   * from the chip's `.hooks`, which is why `.hook` is not it.
   */
  it('wears names neither sheet already reaches', () => {
    expect(draw(), "the row wears the box class the sheet's rules are written for").toContain(
      '<details class="hookrun"',
    );
    for (const [what, sheet] of sheets()) {
      // Every class the row wears, not only the ones it leads with: `.ev` and
      // `.nm` are the kind of short name a later rule picks up by accident.
      for (const name of ['hk', 'nm', 'ev', 'hook']) {
        expect(bareRules(sheet, name), `${what} has no bare .${name} rule`).toEqual([]);
      }
      expect(
        bareRules(sheet, 'hookrun'),
        `${what} spells the row's own box, so its silence means something`,
      ).not.toEqual([]);
    }
    expect(bareRules(SHEET, 'hooks'), 'where the name it must not take is taken').not.toEqual([]);
  });

  it('draws the row in the book, open and closed', () => {
    // The page is the visual truth for both states, and a closed-only drawing is
    // how a surface ends up described in one of them.
    const rows = PAGE.match(/<details class="hookrun"[\s\S]*?<\/details>/g) ?? [];
    expect(rows.length, 'the drawing carries the row').toBeGreaterThan(0);
    expect(
      rows.filter((row) => !row.includes('class="body"')),
      'with a closed one, carrying its state and no body',
    ).not.toEqual([]);
    expect(
      rows.filter((row) => row.includes('open')),
      'and an open one, with what the hook printed',
    ).not.toEqual([]);
  });
});

/** The two sheets a page reads: the app's, and the book's own drawing of it. */
function sheets(): Array<[string, string]> {
  return [
    ['web.css', SHEET],
    ['the book drawing', BOOK],
  ];
}

/**
 * Every selector member whose FIRST compound is that class, wherever it is
 * written.
 *
 * **A rule is not always at the top level and not always alone in its
 * selector.** `@media (min-width: 1px) { .hk { ... } }` reaches the row exactly
 * as a top-level rule does, and `.hk, .other { ... }` is a member of a list a
 * name-only scan walks past.
 */
function bareRules(sheet: string, name: string): string[] {
  const code = sheet.replace(/\/\*[\s\S]*?\*\//g, '');
  const found: string[] = [];
  for (const rule of code.matchAll(/([^{}]+)\{/g)) {
    for (const member of (rule[1] ?? '').split(',')) {
      const first = member.trim().split(/[\s>+~]+/)[0] ?? '';
      if (first === `.${name}`) found.push(member.trim());
    }
  }
  return found;
}
