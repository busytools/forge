import { readFileSync } from 'node:fs';

import { render } from 'svelte/server';
import { describe, expect, it } from 'vitest';

import Hook from './Hook.svelte';
import type { HookRun } from './units';

/**
 * The hook run row, as markup and as rules.
 *
 * **What this can answer and what it cannot.** The row is a disclosure, so its
 * accessible name is its summary and the thing to assert is text: a reader who
 * cannot see it has only those words to know a hook ran and how it ended. The
 * open body is what the fold sent, which is a fact about the markup. Where the
 * mark and the chevron land on the row is layout, which jsdom does not perform.
 *
 * **The rules are read where the sheets are, and each scan says what it read.**
 * Every one of them is a walk over a sheet rather than a file read, so each
 * asserts the denominator it claims: a predicate that reaches no rule reports a
 * clean sheet, which is the answer that looks most like success.
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
    const body = draw();
    expect(body, "the row wears the box class the sheet's rules are written for").toContain(
      '<details class="hookrun"',
    );
    for (const [what, sheet] of sheets()) {
      // Every class the row wears, not only the ones it leads with: `.ev` and
      // `.nm` are the kind of short name a later rule picks up by accident.
      for (const name of WORN) {
        // The markup is where the list is held to the row: a name misspelled
        // here would have no bare rule and no wearer, and would read as clean.
        expect(body, `the row wears .${name}`).toContain(`class="${name}"`);
        expect(bareRules(sheet, name), `${what} has no bare .${name} rule`).toEqual([]);
      }
      expect(
        bareRules(sheet, 'hookrun'),
        `${what} spells the row's own box, so its silence means something`,
      ).not.toEqual([]);
    }
    expect(bareRules(SHEET, 'hooks'), 'where the name it must not take is taken').not.toEqual([]);
  });

  /**
   * The pin the disclosure rests on, and the one rule 25 is about: the row
   * collapses the output behind its own open rather than shortening it, so a
   * clamp anywhere on the body would turn this row back into the drop it was
   * built to stop.
   *
   * **An allowlist, not a denylist.** A denylist answers only the forms someone
   * thought of: `clip-path`, `content-visibility`, `line-height: 0`, `height`
   * and a case variant of a listed name all pass one. Naming what may be
   * declared makes an unknown property the failure instead.
   *
   * It governs WHICH properties the body may be given, not their values - a
   * `color: transparent` is still writable here, and that is the boundary.
   */
  it('declares nothing on the body but what it draws with', () => {
    for (const [what, sheet] of sheets()) {
      const reaching = bodyRules(sheet);
      // The denominator: a scan that reaches no rule reports every sheet clean,
      // and this predicate is a walk over selectors rather than a file read.
      expect(reaching.length, `${what} spells rules reaching the row's body`).toBeGreaterThan(0);
      // Each class, not the set: a list that lost `.term` still finds the
      // `.body` rules and would leave every `.term`-only clamp unseen.
      for (const name of BODY_CLASSES) {
        expect(
          reaching.filter((rule) => rule.reaches.includes(name)).length,
          `${what} spells a rule reaching .${name}`,
        ).toBeGreaterThan(0);
      }
      expect(
        reaching.flatMap((rule) =>
          rule.declarations
            .filter((declaration) => !BODY_PROPERTIES.includes(propertyOf(declaration)))
            .map((declaration) => `${under(rule)} { ${declaration} }`),
        ),
        `${what} declares nothing on the row's body but what it draws with`,
      ).toEqual([]);
    }
  });

  /**
   * The drawing is the surface's visual truth, so a rule edited in one sheet
   * alone is one surface described two ways. Normalised, because the sheets'
   * comments and indentation are each their own.
   *
   * **The set is the row's own rules AND the rules its body draws through**,
   * which are `.body` and `.term`, both shared with other rows: the output
   * wraps by `.term`'s `white-space`, so a divergence there is the drawing
   * showing a reader different output. An at-rule's own prelude is part of the
   * comparison, so a rule stepped into a media query in one sheet alone is not
   * the same rule as a top-level one.
   */
  it("mirrors the row's rules in both sheets, rule for rule", () => {
    const app = rowRules(SHEET);
    // The same denominator one level up: a block the scan failed to read
    // compares equal to an empty one.
    expect(app.length, 'the app spells the row').toBeGreaterThan(0);
    expect(rowRules(BOOK), 'and the drawing mirrors every one of them').toEqual(app);
  });

  it('reads a rule stepped into an at-rule as a different rule', () => {
    // The control the comparison above needs. Without the prelude in the string
    // it compares, the same rule at the top level in one sheet and inside a
    // media query in the other compares EQUAL, and a responsive tweak to this
    // row made in one sheet alone leaves the pin green - which is the shape of
    // instrument the rest of this file exists to refuse.
    const flat = '.hookrun { margin: 3px 0; }';
    const stepped = '@media (min-width: 900px) { .hookrun { margin: 3px 0; } }';

    expect(rowRules(flat), 'a sheet of one plain rule').toEqual(['.hookrun { margin: 3px 0 }']);
    expect(rowRules(stepped), 'and the same rule inside a query').toEqual([
      '@media (min-width: 900px) .hookrun { margin: 3px 0 }',
    ]);
    expect(rowRules(flat), 'which are not the same rule').not.toEqual(rowRules(stepped));
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
 * Every class the row draws with, which is what a bare rule would reach. `hook`
 * is not among them: it is the name the row must NOT take, one letter from the
 * chip's `.hooks`, and is checked as the collision rather than as a wearer.
 */
const WORN = ['hk', 'nm', 'ev'];

/**
 * The classes the row's body draws through. Both are shared with other rows, so
 * a rule written for one of them reaches this one.
 */
const BODY_CLASSES = ['body', 'term'];

/**
 * The properties a rule reaching the row's body may declare, which is what it
 * draws with today: eight, none of which can hide or shorten a line.
 */
const BODY_PROPERTIES = [
  'border-left',
  'color',
  'font-family',
  'font-size',
  'margin',
  'overflow-wrap',
  'padding',
  'white-space',
];

/** One declaration's property, lowercased - CSS property names are not case-sensitive. */
function propertyOf(declaration: string): string {
  return (declaration.split(':')[0] ?? '').trim().toLowerCase();
}

/** A rule as something a reader can be shown, with the at-rule it sits in. */
function under(rule: PlainRule): string {
  return `${rule.prelude === '' ? '' : `${rule.prelude} `}${rule.selector}`;
}

/** One rule as a sheet spells it, before anything asks what it reaches. */
interface PlainRule {
  /** The `@media`-style prelude it sits inside, or an empty string at the top level. */
  prelude: string;
  selector: string;
  declarations: string[];
}

/** One rule, with those of the classes asked for that its selector reaches. */
interface Rule extends PlainRule {
  reaches: string[];
}

/**
 * Every `selector { declarations }` rule in a sheet, with the prelude of any
 * at-rule it sits inside.
 *
 * **A rule is not always at the top level.** The session page's grid is stepped
 * inside media queries, so a row's rules can be too - and a rule stepped into a
 * query in one sheet is not the same rule as a top-level one in the other, which
 * the prelude is what tells apart.
 */
function rulesIn(sheet: string): PlainRule[] {
  const code = sheet.replace(/\/\*[\s\S]*?\*\//g, '');
  const out: PlainRule[] = [];
  const open: string[] = [];
  let held = '';
  let at = 0;
  while (at < code.length) {
    const ch = code[at] ?? '';
    if (ch === '{') {
      const prelude = held.trim();
      held = '';
      at += 1;
      if (prelude.startsWith('@')) {
        open.push(prelude);
        continue;
      }
      let depth = 1;
      const from = at;
      while (at < code.length && depth > 0) {
        if (code[at] === '{') depth += 1;
        else if (code[at] === '}') depth -= 1;
        at += 1;
      }
      out.push({
        prelude: open.join(' '),
        selector: prelude,
        declarations: code
          .slice(from, at - 1)
          .split(';')
          .map((declaration) => declaration.trim())
          .filter((declaration) => declaration !== ''),
      });
      continue;
    }
    if (ch === '}') {
      open.pop();
      held = '';
      at += 1;
      continue;
    }
    held += ch;
    at += 1;
  }
  return out;
}

/**
 * The classes a selector reaches: class compounds only, since a bare `body` in
 * a selector is the page element rather than this row's box.
 */
function classesReached(selector: string): string[] {
  return selector
    .split(',')
    .flatMap((member) => member.split(/[\s>+~]+/))
    .filter((compound) => compound.startsWith('.'))
    .map((compound) => compound.replace(/^\./, '').replace(/::?[\w-]*(\([^)]*\))?$/, ''));
}

/** Every rule whose selector reaches one of the classes the body draws with. */
function bodyRules(sheet: string): Rule[] {
  return rulesIn(sheet).flatMap((rule) => {
    const reaches = classesReached(rule.selector).filter((name) => BODY_CLASSES.includes(name));
    return reaches.length === 0 ? [] : [{ ...rule, reaches }];
  });
}

/**
 * The row's own rules, and the rules its body draws through, as normalised
 * strings: which sheet a rule lives in is the only thing the caller compares.
 */
function rowRules(sheet: string): string[] {
  return rulesIn(sheet)
    .filter(
      (rule) =>
        rule.selector.includes('.hookrun') ||
        classesReached(rule.selector).some((name) => BODY_CLASSES.includes(name)),
    )
    .map((rule) => `${under(rule)} { ${rule.declarations.join('; ').replace(/\s+/g, ' ')} }`)
    .map((rule) => rule.replace(/\s+/g, ' '));
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
  const found: string[] = [];
  for (const rule of rulesIn(sheet)) {
    for (const member of rule.selector.split(',')) {
      const first = member.trim().split(/[\s>+~]+/)[0] ?? '';
      if (first === `.${name}`) found.push(member.trim());
    }
  }
  return found;
}
