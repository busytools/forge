import { readFileSync } from 'node:fs';

import { JSDOM } from 'jsdom';
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
  it('names the hook and its own words on the closed row', () => {
    const body = draw();
    const said = summaryWords(body);

    // The row leads with the name: the lane above it says the kind, so a row
    // repeating "hook" would be the second telling the lane exists to end.
    expect(said, 'the name, and nothing ahead of it').toMatch(/^SessionStart:startup/);
    expect(said, 'the hook the CLI matched, under its own name').toContain('SessionStart:startup');
    expect(said, 'and what it printed, joined, without opening it').toContain('capture-line-1');
    expect(said, 'every line of it, not only the first').toContain('capture-line-2');
    expect(
      summaryOf(draw({ body: null })),
      'a run that printed nothing carries no tail',
    ).not.toContain('class="ev"');
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
   * The tail is what the hook SAID, not what it printed: a session-start hook
   * prints markdown it injects, whose block marks cannot draw on one line, or
   * a JSON envelope whose braces are punctuation rather than words (Ved,
   * 2026-10-03).
   */
  it("draws the tail as the hook's words, marks and envelope read through", () => {
    // A heading's marker would land as raw `##` where the words should be.
    const markdown = summaryWords(draw({ body: '## The PR review gate\n\n- one\n- two\n' }));
    expect(markdown, "the heading's own words").toContain('The PR review gate');
    expect(markdown, 'without the block mark that cannot draw').not.toContain('##');
    expect(markdown, "and the list's own lines").toContain('one');

    // Inline marks still draw, the way the body and a thought's line do.
    expect(
      draw({ body: 'the **fold** joins, `cargo check` runs' }),
      'inline marks rendered',
    ).toContain('<strong>fold</strong>');

    // The envelope the CLI reads: its braces are not the hook's words.
    const envelope = JSON.stringify({
      hookSpecificOutput: {
        hookEventName: 'SessionStart',
        additionalContext: 'git_platform: github, cli: gh',
      },
    });
    const inner = summaryWords(draw({ body: envelope }));
    expect(inner, 'the context the envelope carried').toContain('git_platform: github, cli: gh');
    expect(inner, 'and none of its punctuation').not.toContain('{');

    // A message for the reader rides the same envelope, and an envelope whose
    // text is empty is a hook that said nothing: no tail rather than a brace.
    const message = JSON.stringify({ systemMessage: 'the tree is dirty' });
    expect(summaryWords(draw({ body: message })), "the envelope's own message").toContain(
      'the tree is dirty',
    );
    const empty = JSON.stringify({
      hookSpecificOutput: { hookEventName: 'SessionStart', additionalContext: '' },
    });
    expect(
      summaryOf(draw({ body: empty })),
      'an envelope that carried nothing carries no tail',
    ).not.toContain('class="ev"');

    // **And only the CLI's own shapes are read.** A key in the half the CLI
    // does not read it from is a hook the session never saw anything from, so
    // it draws no tail rather than words the reader was never shown.
    const misShaped = JSON.stringify({ additionalContext: 'never read by the CLI' });
    expect(
      summaryOf(draw({ body: misShaped })),
      'a key the CLI does not read from the top level draws no tail',
    ).not.toContain('class="ev"');

    // **And the tail is one line whatever the output weighs.** The thought
    // row's own line is the precedent: the words joined, the layout's
    // ellipsis where they run out, the whole of it behind the open.
    for (const [what, sheet] of sheets()) {
      expect(sheet, `${what} holds the row's tail to one line`).toMatch(
        /details\.hookrow > summary \.ev \{[^}]*overflow: hidden[^}]*text-overflow: ellipsis[^}]*white-space: nowrap/,
      );
      // **And the cut never takes the name's letters.** Two clamped spans
      // share the flex shrink, and a long tail shrank `SessionStart:startup`
      // to `Se...` until the name was held at its own width (Ved, 2026-10-03).
      expect(sheet, `${what} keeps the row's name at its own width`).toMatch(
        /details\.hookrow > summary \.tn \{[^}]*flex: none/,
      );
      // A mark in the tail reads as a mark in the prose tone, the way the
      // name's own marks do - the tail's muting is for the words around it.
      expect(sheet, `${what} draws the tail's marks in the prose tone`).toMatch(
        /details\.hookrow > summary \.ev (?:strong|code)[^{]*\{[^}]*color: var\(--text\)/,
      );
    }
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
        /details\.leaf > summary \.st\.err \{[^}]*color: var\(--bad\)/,
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
   * of. So every class this row wears must have no BARE rule of that name, and
   * the row's own box is one letter from the chip's `.hooks`, which is why
   * `.hook` is not it.
   *
   * **Bare is what this reads**: a rule for the same name under another
   * component's ancestor (`details.kind > summary .nm`) is a different rule and
   * is not this check's, which is why the rendered check is the one that says
   * what actually lands.
   */
  it('wears names neither sheet already reaches', () => {
    const body = draw();
    // The row is a tool row: it wears the shared leaf box, so the sheet's own
    // leaf rules (and nothing of another kind) are what draws it.
    expect(body, 'the row wears the shared tool-row box').toContain(
      '<details class="leaf hookrow"',
    );
    for (const [what, sheet] of sheets()) {
      // Every class the row wears, not only the ones it leads with: `.ev` is
      // the kind of short name a later rule picks up by accident.
      for (const name of WORN) {
        // The markup is where the list is held to the row: a name misspelled
        // here would have no bare rule and no wearer, and would read as clean.
        expect(body, `the row wears .${name}`).toMatch(new RegExp(`class="[^"]*\\b${name}\\b`));
        expect(bareRules(sheet, name), `${what} has no bare .${name} rule`).toEqual([]);
      }
      // The control the loop above needs: the scan that reports no bare rule
      // would also report no rule at all, so the sheet has to be seen to carry
      // the row's own class before its silence about a bare one means anything.
      expect(
        sheet,
        `${what} spells the row's own state class, so its silence means something`,
      ).toMatch(/details\.hookrow > summary \.ev/);
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
   * the two - scoping a tweak to this row is the point - and `details.hookrow >
   * div` is the body without naming it, so the scope is the row and everything
   * that reaches it. A rule touching both scopes takes the body's tighter list.
   *
   * **What neither list can reach is a rule that answers to none of the row's
   * classes** - `details > div` clamps this body and every other disclosure's.
   * That half is `gives the body nothing the row does not`, which asks the
   * rendered body rather than the selector, because a selector's reach cannot
   * be read off its text.
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
        reaching.filter((rule) => rule.selector.includes('.hookrow')).length,
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
   * What the elements the body draws through are actually given, with a sheet
   * applied.
   *
   * **Decidable rather than textual.** Three rounds of this check were each
   * closed by narrowing a matcher - the property list, the class matcher, the
   * rule selection - and each narrowing exposed the next spelling, because a
   * selector's reach is not readable off its text. So this one renders the row
   * and asks those elements what they got, against a control holding every rule
   * EXCEPT the ones whose SUBJECT answers for neither the row nor what wraps it.
   *
   * Anything they are given that the control does not give them came in
   * sideways, which is the whole of what the lists above cannot see - including
   * a rule aimed one element deeper than the body div, and one hidden inside an
   * at-rule, both of which the render now reaches.
   *
   * **The inheritance axis is read through `INHERITED` alone.** A wrapper's own
   * declarations do reach these elements - that is why the wrappers are kept in
   * the control - but jsdom enumerates none of them on a child, so what a rule
   * on an ancestor hides is only read where it hides by inheriting or
   * compositing. A rule that hides the whole turn is the turn's defect and not
   * this row's; this check is for what reaches the row's own output.
   */
  it('gives the body nothing the row does not', () => {
    for (const [what, sheet] of sheets()) {
      const rules = rulesIn(sheet);
      const real = bodyStyle(renderable(rules));
      const control = bodyStyle(renderable(rules.filter((rule) => answersFor(rule.selector))));
      // The denominators, both sides: an element the render never reached
      // compares equal to anything, and a control that kept nothing would make
      // every sheet look like it gave the body everything.
      expect(Object.keys(real).length, `${what} gives the body something to read`).toBeGreaterThan(
        0,
      );
      expect(
        Object.keys(control).length,
        `${what} gives the control something to compare against`,
      ).toBeGreaterThan(0);
      expect(real, `${what} gives the body only what the row answers for`).toEqual(control);
    }
  });

  /**
   * The control's own filter, pinned in both directions.
   *
   * **The one matcher in this file that decides another check's input**, so it
   * is the one that most needs saying what it reads: the subject, never the
   * reach. A rule that reaches the row without answering for it must stay OUT
   * of the control, and a rule for the row or its surroundings must stay in, or
   * the comparison either hides a hole or reds a good sheet.
   */
  it('keeps the control to what answers for the row', () => {
    for (const kept of [
      '.hookrow',
      'details.hookrow',
      '.hookrow:hover',
      '.body',
      '.term',
      '.conv',
      ':root',
    ]) {
      expect(answersFor(kept), `${kept} answers for the row or what wraps it`).toBe(true);
    }
    // The subject is what decides, not the ancestor: a rule written FOR the
    // summary or for a div is kept out even when the row is in its selector,
    // because what it styles is not what this comparison reads.
    for (const dropped of [
      'details > div',
      'details.hookrow > div',
      'details.hookrow > summary',
      'div',
      '.nm',
      '.ev',
      '.term .pfx',
    ]) {
      expect(answersFor(dropped), `${dropped} answers for neither`).toBe(false);
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
    const flat = '.hookrow { margin: 3px 0; }';
    const stepped = '@media (min-width: 900px) { .hookrow { margin: 3px 0; } }';
    // And the other shape an at-rule has: one that ends at its semicolon owns no
    // block, and left in the walker's buffer it becomes the next rule's prelude.
    const stated = '@import url("x.css");\n.hookrow { margin: 3px 0; }';

    expect(rowRules(flat).map(ruleText), 'a sheet of one plain rule').toEqual([
      '.hookrow { margin: 3px 0 }',
    ]);
    expect(rowRules(stepped).map(ruleText), 'and the same rule inside a query').toEqual([
      '@media (min-width: 900px) .hookrow { margin: 3px 0 }',
    ]);
    expect(rowRules(stated).map(ruleText), 'and one after a statement at-rule').toEqual([
      '.hookrow { margin: 3px 0 }',
    ]);
    expect(rowRules(flat), 'which are not the same rule').not.toEqual(rowRules(stepped));
  });

  it('reads a selector that spells a class away from its compound', () => {
    // The control the matcher needs, and the one this check has been caught
    // without twice: `div.body` and `.body.clamped` NAME the body's class
    // wherever the class sits in the compound, and a matcher reading only the
    // first class let both past the guard and the mirror alike. What this says
    // is what the matcher reads, not what the cascade applies - `.body.clamped`
    // needs an element wearing both classes, which today's markup has not got.
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
    expect(reachesRow('details.hookrow > div'), 'a row-scoped rule reaches the row').toBe(true);
    expect(
      reachesBody('details.hookrow > div'),
      "and names none of the body's classes, which is why the scope has to be the row",
    ).toEqual([]);
  });

  it('draws the row in the book, open and closed', () => {
    // The page is the visual truth for both states, and a closed-only drawing is
    // how a surface ends up described in one of them.
    const rows = PAGE.match(/<details class="leaf hookrow"[\s\S]*?<\/details>/g) ?? [];
    expect(rows.length, 'the drawing carries the row').toBeGreaterThan(0);
    expect(
      rows.filter((row) => !row.includes('class="body"')),
      'with a closed one, carrying its words and no body',
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
const WORN = ['hookrow', 'ev'];

/**
 * The class the row's own box wears, and the classes its body draws through.
 * The two body classes are shared with other rows, so a rule written for one of
 * them reaches this one.
 */
const ROW_CLASS = 'hookrow';
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

const ROW_PROPERTIES = ['color', 'flex', 'overflow', 'text-overflow', 'white-space'];

/**
 * Every class token a selector names, wherever inside the selector it sits.
 *
 * **Not the leading compound's name.** `div.body`, `.body.clamped` and
 * `:is(.body)` all name the body's class, and a matcher that read only the
 * first class of each compound let every one of them past both the property
 * guard and the mirror - a clamp written as `div.body` needed no markup change
 * to hide the output this row exists to carry. The selector is where a check
 * like this is attacked, so the matcher reads every token.
 *
 * **Bounded by that regex, and the bound is the point of the rendered check.**
 * A class spelled as an attribute (`[class~="body"]`) or with an escape names
 * the same class and matches no `.`-token, so this reads nothing there; the
 * cascade does not care how the sheet spells it, which is why
 * `gives the body nothing the row does not` is the one that settles it.
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

/**
 * The elements this test itself puts around the row, and the root the sheets'
 * own tokens hang off. Rules for these are kept in the control so the row's own
 * rules are compared in the context they render in.
 */
const SURROUNDINGS = ['conv', 'work', ':root', 'html', 'body'];

/**
 * The properties a subtree can be hidden by WITHOUT being set on it: one
 * inherits, one composites everything under it. jsdom's enumeration names
 * neither on a child, so the read below asks for them by name.
 */
const INHERITED = ['visibility', 'opacity'];

/**
 * A rule's subject - the element it styles - with any pseudo taken off.
 *
 * **`::` and a trailing pseudo-class come off; a lone `:root` does not.** A
 * strip that removed every leading `:` turned `:root` into an empty string, so
 * the one rule the sheets' tokens hang off read as answering for nothing and
 * was dropped from the control.
 */
function subjectOf(member: string): string {
  const raw =
    member
      .trim()
      .split(/[\s>+~]+/)
      .pop() ?? '';
  const stripped = raw.replace(/::?[\w-]*(\([^)]*\))?$/, '');
  return stripped === '' ? raw : stripped;
}

/**
 * Whether a rule is written for the row, or for what this test wraps it in.
 *
 * **The subject alone, never whether the rule reaches the row.** A rule that
 * reaches it secretly - `details > div` clamps this body and no class matcher
 * can say so - is exactly what the control exists to expose, and a filter that
 * asked the same matcher the guard asks would keep it and hide it.
 */
function answersFor(selector: string): boolean {
  const own = [ROW_CLASS, ...BODY_CLASSES, ...SURROUNDINGS];
  return selector.split(',').every((member) => {
    const subject = subjectOf(member);
    return classesIn(subject).some((name) => own.includes(name)) || own.includes(subject);
  });
}

/**
 * Rules as a stylesheet, every one at the top level.
 *
 * **The at-rules are unwrapped on purpose.** jsdom reads a media query's type
 * and evaluates none of its features - measured: `@media (min-width: 1px)`
 * does not apply and `@media screen` does - so a rule inside a feature query
 * would never enter the comparison at all, and a clamp hidden in one would
 * read as clean. Hoisted, it applies, and the check answers the question the
 * guard claims to: what would this rule do to the body.
 */
function renderable(rules: readonly PlainRule[]): string {
  return rules.map((rule) => `${rule.selector} { ${rule.declarations.join('; ')} }`).join('\n');
}

/** The row as the page wraps it, which is what a sheet's ancestors match against. */
const ROW_MARKUP = `<div class="conv"><div class="work">${
  render(Hook, { props: { run: run() } }).body
}</div></div>`;

/**
 * What the cascade sets on every element the row's body draws through, by
 * element and property.
 *
 * **Every element, not only the body div.** The hook's words sit in `div.term`
 * inside it, so a rule aimed one element deeper - `details > div > div` - lands
 * on the element holding the text and matches nothing a `.body`-only query
 * looks at. The set read is the classes this file already declares the body
 * draws through, which is also the set the allowlist guards.
 *
 * **What jsdom does not do, stated because the check rests on it**: it
 * evaluates no media features (hence `renderable`), and it enumerates no
 * inherited property on a child (hence `INHERITED`). Both are 30.1.1's
 * behaviour rather than a contract, and the lockfile is what holds it still.
 */
function bodyStyle(sheet: string): Record<string, string> {
  const dom = new JSDOM(
    `<!doctype html><html><head><style>${sheet}</style></head><body>${ROW_MARKUP}</body></html>`,
    { pretendToBeVisual: true },
  );
  const win = dom.window;
  const row = win.document.querySelector('.hookrow');
  if (row === null) throw new Error('the row did not render for the sheet to reach');
  const out: Record<string, string> = {};
  for (const element of row.querySelectorAll('[class]')) {
    const names = [...element.classList].filter((name) => BODY_CLASSES.includes(name));
    if (names.length === 0) continue;
    const computed = win.getComputedStyle(element);
    const read = (name: string): void => {
      out[`${names.join('.')}:${name}`] = computed.getPropertyValue(name);
    };
    for (let at = 0; at < computed.length; at += 1) read(computed.item(at));
    for (const name of INHERITED) read(name);
  }
  return out;
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
 * selector.** `@media (min-width: 1px) { .hk { ... } }` is READ here as the
 * rule it is, with its prelude kept, and `.hk, .other { ... }` is a member of a
 * list a name-only scan walks past.
 *
 * **Whether a query-wrapped rule APPLIES is a different question and not this
 * one**: jsdom evaluates no media feature, so `renderable` hoists every at-rule
 * body to the top level before the render, which is what makes the comparison
 * see them.
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
