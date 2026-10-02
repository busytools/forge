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
   * clamp anywhere on it would turn this row back into the drop it was built to
   * stop.
   *
   * **An allowlist, not a denylist.** A denylist answers only the forms someone
   * thought of: `clip-path`, `content-visibility`, `line-height: 0`, `height`
   * and a case variant of a listed name all pass one. Naming what may be
   * declared makes an unknown property the failure instead.
   *
   * **It guards every rule that can reach the row, not only the rules that name
   * the body's classes.** A clamp scoped to the row is the likelier mistake of
   * the two - scoping a tweak to this row is the point - and `details.hookrun >
   * div` is the body without naming it, so the scope is the row and everything
   * that reaches it. A rule touching both scopes takes the body's tighter list.
   *
   * It governs WHICH properties may be declared, not their values: a
   * `color: transparent` is writable here, and that is the boundary.
   */
  it('declares nothing on the row but what it and its body draw with', () => {
    for (const [what, sheet] of sheets()) {
      const reaching = rowRules(sheet);
      // The denominator: a scan that reaches no rule reports every sheet clean,
      // and this predicate is a walk over selectors rather than a file read.
      expect(reaching.length, `${what} spells rules reaching the row`).toBeGreaterThan(0);
      // Each arm of the set, and each body class, rather than the set as a
      // whole: a matcher that lost the body's classes still finds the row's own
      // rules, and one that lost `.term` still finds `.body`.
      expect(
        reaching.filter((rule) => reachesBody(rule.selector).length > 0).length,
        `${what} spells a rule reaching the body's own classes`,
      ).toBeGreaterThan(0);
      expect(
        reaching.filter((rule) => rule.selector.includes('.hookrun')).length,
        `${what} spells the row's own rules`,
      ).toBeGreaterThan(0);
      for (const name of BODY_CLASSES) {
        expect(
          reaching.filter((rule) => reachesBody(rule.selector).includes(name)).length,
          `${what} spells a rule reaching .${name}`,
        ).toBeGreaterThan(0);
      }
      expect(
        reaching.flatMap((rule) =>
          rule.declarations
            .filter((declaration) => !allowedIn(rule, declaration))
            .map((declaration) => `${under(rule)} { ${declaration} }`),
        ),
        `${what} declares nothing on the row but what it draws with`,
      ).toEqual([]);
    }
  });

  /**
   * The allowlists held to the sheets that justify them.
   *
   * **Widening one is otherwise silent**: adding a property to the list and the
   * declaration that needs it leaves every other check green, and the list is
   * the guard. Read both ways, an entry with nothing behind it fails and a
   * declaration with no entry fails, so the list can only move with the sheet.
   */
  it('holds each allowlist to what its own sheet declares', () => {
    for (const [what, sheet] of sheets()) {
      expect(
        declaredProperties(bodyRules(sheet)),
        `${what} declares exactly the properties of the body's classes`,
      ).toEqual([...BODY_PROPERTIES].sort());
      expect(
        declaredProperties(bodylessRules(sheet)),
        `${what} declares exactly the properties of the row's own rules`,
      ).toEqual([...ROW_PROPERTIES].sort());
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
    // The same denominator one level up, and again per arm: a block the scan
    // failed to read compares equal to an empty one, and a set that had lost
    // the rules the body draws through would compare equal while those rules
    // diverged between the sheets.
    expect(app.length, 'the app spells the row').toBeGreaterThan(0);
    expect(
      app.filter((rule) => reachesBody(rule.selector).length > 0).length,
      'the app spells the rules the body draws through, so their silence means something',
    ).toBeGreaterThan(0);
    expect(
      app.filter((rule) => classesIn(rule.selector).includes(ROW_CLASS)).length,
      "and the row's own rules",
    ).toBeGreaterThan(0);
    expect(rowRules(BOOK).map(ruleText), 'and the drawing mirrors every one of them').toEqual(
      app.map(ruleText),
    );
  });

  it('reads a rule stepped into an at-rule as a different rule', () => {
    // The control the comparison above needs. Without the prelude in the string
    // it compares, the same rule at the top level in one sheet and inside a
    // media query in the other compares EQUAL, and a responsive tweak to this
    // row made in one sheet alone leaves the pin green - which is the shape of
    // instrument the rest of this file exists to refuse.
    const flat = '.hookrun { margin: 3px 0; }';
    const stepped = '@media (min-width: 900px) { .hookrun { margin: 3px 0; } }';
    // And the other shape an at-rule has: one that ends at its semicolon owns no
    // block, and left in the walker's buffer it becomes the next rule's prelude.
    const stated = '@import url("x.css");\n.hookrun { margin: 3px 0; }';

    expect(rowRules(flat).map(ruleText), 'a sheet of one plain rule').toEqual([
      '.hookrun { margin: 3px 0 }',
    ]);
    expect(rowRules(stepped).map(ruleText), 'and the same rule inside a query').toEqual([
      '@media (min-width: 900px) .hookrun { margin: 3px 0 }',
    ]);
    expect(rowRules(stated).map(ruleText), 'and one after a statement at-rule').toEqual([
      '.hookrun { margin: 3px 0 }',
    ]);
    expect(rowRules(flat), 'which are not the same rule').not.toEqual(rowRules(stepped));
  });

  it('reads a selector that spells a class away from its compound', () => {
    // The control the matcher needs, and the one this check has been caught
    // without twice: `div.body` and `.body.clamped` reach the same box as a
    // bare `.body`, and a matcher reading only the first class of each compound
    // lets both past the guard and the mirror alike.
    const spellings: Array<[string, string]> = [
      ['div.body', 'body'],
      ['.body.clamped', 'body'],
      [':is(.body)', 'body'],
      ['.term.fail', 'term'],
    ];

    for (const [spelling, name] of spellings) {
      expect(classesIn(spelling), `the classes ${spelling} names`).toContain(name);
      expect(
        bodyRules(`${spelling} { color: var(--dim); }`).length,
        `${spelling} reaches the body's classes`,
      ).toBeGreaterThan(0);
    }
    expect(reachesRow('details.hookrun > div'), 'a row-scoped rule reaches the row').toBe(true);
    expect(
      reachesBody('details.hookrun > div'),
      "and names none of the body's classes, which is why the scope has to be the row",
    ).toEqual([]);
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
 * The class the row's own box wears, and the classes its body draws through.
 * The two body classes are shared with other rows, so a rule written for one of
 * them reaches this one.
 */
const ROW_CLASS = 'hookrun';
const BODY_CLASSES = ['body', 'term'];

/**
 * The properties a rule may declare, by the scope it is written for: what the
 * body's own classes carry, which is what the output is drawn with, and what
 * the row's block carries, which is what the row is.
 *
 * Both are the whole of what the sheets declare for that scope today, and
 * `holds each allowlist to what its own sheet declares` is what keeps them
 * there.
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

const ROW_PROPERTIES = [
  'align-items',
  'color',
  'cursor',
  'display',
  'flex',
  'font-family',
  'font-size',
  'font-weight',
  'gap',
  'list-style',
  'margin',
  'margin-left',
  'white-space',
];

/**
 * Every class token a selector names, wherever inside the selector it sits.
 *
 * **Not the leading compound's name.** `div.body`, `.body.clamped` and
 * `:is(.body)` all reach the same box as a bare `.body`, and a matcher that
 * read only the first class of each compound let every one of them past both
 * the property guard and the mirror - a clamp written as `div.body` needed no
 * markup change to hide the output this row exists to carry. The selector is
 * where a check like this is attacked, so the matcher reads every token.
 */
function classesIn(selector: string): string[] {
  return [...selector.matchAll(/\.([\w-]+)/g)].map(([, name = '']) => name);
}

/** The classes of the body's own that a selector names, if any. */
function reachesBody(selector: string): string[] {
  return classesIn(selector).filter((name) => BODY_CLASSES.includes(name));
}

/** Whether a selector can reach what the row draws, by its own class or the body's. */
function reachesRow(selector: string): boolean {
  const classes = classesIn(selector);
  return classes.includes(ROW_CLASS) || classes.some((name) => BODY_CLASSES.includes(name));
}

/** Every rule whose selector reaches the classes the body draws with. */
function bodyRules(sheet: string): PlainRule[] {
  return rulesIn(sheet).filter((rule) => reachesBody(rule.selector).length > 0);
}

/** Every rule that can reach what the row draws, by either route. */
function rowRules(sheet: string): PlainRule[] {
  return rulesIn(sheet).filter((rule) => reachesRow(rule.selector));
}

/** The row's own rules: the ones that reach it without the body's classes. */
function bodylessRules(sheet: string): PlainRule[] {
  return rowRules(sheet).filter((rule) => reachesBody(rule.selector).length === 0);
}

/**
 * Whether a declaration is one the rule's own scope may make. A rule touching
 * both scopes takes the body's tighter list, which is the safe direction.
 */
function allowedIn(rule: PlainRule, declaration: string): boolean {
  const scope = reachesBody(rule.selector).length > 0 ? BODY_PROPERTIES : ROW_PROPERTIES;
  return scope.includes(propertyOf(declaration));
}

/** The distinct properties a set of rules declares, sorted. */
function declaredProperties(rules: PlainRule[]): string[] {
  return [...new Set(rules.flatMap((rule) => rule.declarations.map(propertyOf)))].sort();
}

/** One declaration's property, lowercased - CSS property names are not case-sensitive. */
function propertyOf(declaration: string): string {
  return (declaration.split(':')[0] ?? '').trim().toLowerCase();
}

/** A rule's own name, with the at-rule it sits in. */
function under(rule: PlainRule): string {
  return `${rule.prelude === '' ? '' : `${rule.prelude} `}${rule.selector}`;
}

/** A whole rule as one normalised line: which sheet it lives in is all that differs. */
function ruleText(rule: PlainRule): string {
  return `${under(rule)} { ${rule.declarations.join('; ').replace(/\s+/g, ' ')} }`.replace(
    /\s+/g,
    ' ',
  );
}

/** One rule as a sheet spells it, before anything asks what it reaches. */
interface PlainRule {
  /** The `@media`-style prelude it sits inside, or an empty string at the top level. */
  prelude: string;
  selector: string;
  declarations: string[];
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
    if (ch === ';' && held.trim().startsWith('@')) {
      // A statement at-rule ends at its semicolon and owns no block. Left in
      // the buffer it would become the next rule's prelude, which is a rule
      // read as something it is not.
      held = '';
      at += 1;
      continue;
    }
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
