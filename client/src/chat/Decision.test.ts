import { readFileSync } from 'node:fs';

import { render } from 'svelte/server';
import { describe, expect, it } from 'vitest';

import { repoPath } from '../repo';
import Decision from './Decision.svelte';
import type { Decision as Parsed } from './decisions';

/** The book's own specimen of this block, which draws the same vocabulary. */
const PAGE = readFileSync(repoPath('docs/book/src/ui/client/web-session.html'), 'utf8');

/** The same page with the entity its separators are written as decoded. */
const PAGE_TEXT = PAGE.replaceAll('&#183;', '\u{b7}');

/** One decision, as the fold hands it to the block. */
const decision = (over: Partial<Parsed>): Parsed => ({
  model: 'jev-1.13.0',
  usage: { input_tokens: 392, output_tokens: 20, cost: null },
  answer: { kind: 'noul', noul: 0.93 },
  question: null,
  criteria: {},
  state: null,
  chips: [{ text: 'yes', tone: 'ok' }],
  figure: '0.93',
  ...over,
});

/**
 * The block, as the reader sees it.
 *
 * Svelte's own block markers come off first: they sit between a `{#if}`'s
 * arms, and an assertion on what the block composes should read the way a
 * browser does rather than counting markers.
 */
const drawn = (over: Partial<Parsed>): string =>
  render(Decision, { props: { decision: decision(over) } }).body.replaceAll(/<!--[\s\S]*?-->/g, '');

/** The option names the list draws, in the order they draw. */
const optionNames = (body: string): string[] =>
  [...body.matchAll(/class="fam-sub">([^<]*)</g)].map((match) => match[1] ?? '');

describe('the block one decision draws', () => {
  it('quotes the whole question under its own tag, its own lines kept', () => {
    const body = drawn({
      question: {
        text: 'Which team owns this?\n- payments owns the ledger\n- frontend owns the surface',
        raw: null,
      },
    });
    expect(body, 'the tag names what the quote is').toContain(
      '<div class="fam-tag">question</div>',
    );
    // The family's own quote: markdown with the lines preserved, so what the
    // caller wrote - a list, a break - is what the page draws. `renderInline`
    // emitted the lines and the page collapsed them.
    expect(body, 'the quote draws as the family renders one').toContain(
      'class="fam-quote"><div class="prose">',
    );
    expect(body, 'the list the caller wrote draws as a list').toContain(
      '<li>payments owns the ledger</li>',
    );
    expect(
      drawn({ question: { text: 'Is this mechanical?\nAnd nothing else.', raw: null } }),
      'and a plain line break is a break on the page',
    ).toContain('Is this mechanical?<br>');
    expect(drawn({}), 'a call that asked nothing draws no question tag').not.toContain(
      '<div class="fam-tag">question</div>',
    );
  });

  it('states the answer as a facts line: the label, then the fact', () => {
    expect(drawn({}), 'a noul states its number and the word it reads as').toContain(
      '<span class="lb">noul</span>0.93<span class="sep">\u{b7}</span><span class="lb">reads</span>yes',
    );
    expect(
      drawn({
        answer: {
          kind: 'choice',
          choice: 'payments',
          probabilities: [{ name: 'payments', value: 0.84 }],
          confidence: 0.72,
        },
      }),
      'a choice its pick and its confidence',
    ).toContain(
      '<span class="lb">pick</span>payments<span class="sep">\u{b7}</span><span class="lb">confidence</span>0.72',
    );
    expect(
      drawn({
        answer: {
          kind: 'score',
          score: 1.79,
          levels: [
            { name: 'Routine', value: 0.08, raw: null },
            { name: 'Soon', value: 0.31, raw: null },
            { name: 'Urgent', value: 0.61, raw: null },
          ],
          confidence: 0.5,
        },
      }),
      'a score where it landed, with the confidence the old card parsed and never drew',
    ).toContain(
      '<span class="lb">score</span>1.79 of 2<span class="sep">\u{b7}</span><span class="lb">confidence</span>0.5',
    );
  });

  it('draws the options as a plain list: the given order, the winner the one bright row, no bars', () => {
    const body = drawn({
      answer: {
        kind: 'choice',
        choice: 'payments',
        probabilities: [
          { name: 'payments', value: 0.84 },
          { name: 'frontend', value: 0.16 },
        ],
        confidence: null,
      },
      criteria: {
        payments: { text: 'owns the ledger', raw: null },
        frontend: { text: 'the surface is the web client only', raw: null },
      },
    });
    expect(body, 'the rows draw as a list').toContain('class="fam-list"');
    expect(optionNames(body), 'in the order the read handed them over').toEqual([
      'payments',
      'frontend',
    ]);
    expect([...body.matchAll(/fam-opt win/g)], 'one bright row, the pick').toHaveLength(1);
    expect(body, 'every option draws its value as text').toContain('class="fam-when">0.16');
    expect(body, 'the description under its own option').toContain(
      'class="fam-desc">owns the ledger',
    );
    expect(body, 'and no bar anywhere').not.toContain('class="track"');
  });

  it('draws a bare pick with no distribution and no confidence', () => {
    const body = drawn({
      answer: { kind: 'choice', choice: 'payments', probabilities: null, confidence: null },
    });
    expect(body, 'the pick still draws').toContain('<span class="lb">pick</span>payments');
    expect(body, 'no option rows').not.toContain('class="fam-list"');
    expect(body, 'and no confidence clause').not.toContain('confidence');
  });

  it('draws a score between its levels, both carrying the mark', () => {
    const body = drawn({
      answer: {
        kind: 'score',
        score: 1.79,
        levels: [
          { name: 'Routine', value: 0.08, raw: null },
          { name: 'Soon', value: 0.31, raw: null },
          { name: 'Urgent', value: 0.61, raw: null },
        ],
        confidence: null,
      },
    });
    expect(body, 'where it landed, of the last level index').toContain('1.79 of 2');
    expect(
      [...body.matchAll(/fam-opt win/g)],
      'the two levels it sits between carry the mark',
    ).toHaveLength(2);
  });

  it('draws a level the result left without a probability, which the facts line still counts', () => {
    // One row per level: skipping the value-less one made the facts line count
    // levels the list did not draw.
    const body = drawn({
      answer: {
        kind: 'score',
        score: 1.5,
        levels: [
          { name: 'Routine', value: 0.5, raw: null },
          { name: 'Soon', value: null, raw: null },
          { name: 'Urgent', value: 0.4, raw: null },
        ],
        confidence: null,
      },
    });
    expect(optionNames(body), 'every level draws its row').toEqual(['Routine', 'Soon', 'Urgent']);
    expect(body, 'with no value text where none was reported').not.toContain('>null<');
  });

  it('draws a score with no levels as its number alone', () => {
    const body = drawn({ answer: { kind: 'score', score: 1.79, levels: null, confidence: null } });
    expect(body).toContain('1.79');
    expect(body, 'no count of levels').not.toContain('of 2');
    expect(body, 'and no option rows').not.toContain('class="fam-list"');
  });

  it('keeps the state behind one line naming its size', () => {
    // What waits behind the fold is the mount test's to read: the material
    // is drawn onto the open, like every other disclosure on the page.
    const body = drawn({
      state: {
        kind: 'lines',
        lines: [{ key: 'claim', text: 'the PR is ready', depth: 0, dim: false }],
        size: '1 field',
      },
    });
    expect(body, 'the fold line names what is behind it').toContain('state - 1 field</summary>');
    expect(body, 'and the material itself waits behind the open').not.toContain('the PR is ready');
    expect(drawn({ state: null }), 'a call that carried no state draws no fold').not.toContain(
      'class="fold"',
    );
  });

  it('names a string state by its size, in characters', () => {
    const body = drawn({
      state: { kind: 'prose', text: 'the ticket moves to payments', size: '28 chars' },
    });
    expect(body, 'the size the fold line names').toContain('state - 28 chars');
  });

  it('names a primitive state without a size, which is the whole of it', () => {
    const body = drawn({ state: { kind: 'prose', text: '3', size: null } });
    expect(body, 'the fold line still names the state').toContain('state</summary>');
    expect(body, 'with no count invented for it').not.toContain('state - ');
  });

  it('states what answered and what it cost along the bottom', () => {
    const body = drawn({ usage: { input_tokens: 280, output_tokens: 20, cost: 0.00001176 } });
    expect(body, 'the model').toContain('<span class="lb">model</span>jev-1.13.0');
    expect(body, 'the tokens').toContain('<span class="lb">tokens</span>280 in / 20 out');
    expect(body, 'and the cost').toContain('<span class="lb">cost</span>$0.00001176');
    expect(
      drawn({ usage: { input_tokens: 280, output_tokens: 20, cost: null } }),
      'no cost segment where the provider reported none',
    ).not.toContain('$');
    expect(
      drawn({ usage: null, model: '' }),
      'nothing to state, so the run does not draw',
    ).not.toContain('class="fam-meta">');
  });

  it("composes the same facts line the book's score specimen draws", () => {
    // The page's specimen and the block are composed from the same words, so
    // the page can only say what an input could produce.
    const body = drawn({
      answer: {
        kind: 'score',
        score: 1.79,
        levels: [
          { name: 'Routine - no deadline pressure', value: 0.08, raw: null },
          { name: 'Soon - worth doing this week', value: 0.31, raw: null },
          { name: 'Urgent - blocks others right now', value: 0.61, raw: null },
        ],
        confidence: 0.62,
      },
    });

    const facts = /<div class="fam-meta answer">([\s\S]*?)<\/div>/.exec(body)?.[1] ?? '';
    expect(facts, 'the block composes a facts line').not.toBe('');
    expect(PAGE_TEXT, 'and the page draws it markup for markup').toContain(facts);
  });

  it("composes the same fold line the book's state specimen draws", () => {
    const body = drawn({
      state: {
        kind: 'lines',
        lines: [
          { key: 'claim', text: 'the fix is a single import line', depth: 0, dim: false },
          { key: 'evidence', text: 'covered by tests', depth: 0, dim: false },
        ],
        size: '2 fields',
      },
    });
    expect(body, 'the block composes the fold line').toContain('state - 2 fields</summary>');
    expect(PAGE, 'and the page draws it word for word').toContain('state - 2 fields');
  });

  it('draws a structured question as its line, with the value one click away', () => {
    const body = drawn({
      question: {
        text: 'Is the claim `just check` green?',
        raw: {
          question: 'Is the claim `just check` green?',
          evidence: { verdict: 'all green' },
        },
      },
    });
    expect(body, 'the naming field draws as the line').toContain(
      'Is the claim <code>just check</code> green?',
    );
    expect(body, 'and the disclosure says what it holds').toContain(
      'structured instructions - 2 fields',
    );
    // The value itself waits behind the disclosure's own open; what the row
    // composes closed is the label (`Decision.mount` reads the open).
    expect(body, 'with the value behind the open, not under the row').not.toContain('all green');
  });

  it('draws a structured criterion behind its own disclosure, and a string one without', () => {
    const body = drawn({
      answer: {
        kind: 'choice',
        choice: 'billing',
        probabilities: [
          { name: 'billing', value: 0.84 },
          { name: 'frontend', value: 0.16 },
        ],
        confidence: null,
      },
      criteria: {
        billing: {
          text: 'owns the ledger and the settlement path',
          raw: { label: 'owns the ledger and the settlement path', files: ['a.rs'] },
        },
        frontend: { text: 'the surface is the web client only', raw: null },
      },
    });
    expect(body, 'the named text draws where a description always did').toContain(
      'owns the ledger and the settlement path',
    );
    expect(body, 'the disclosure is labelled by what it holds').toContain(
      'structured value - 2 fields',
    );
    expect(body, 'and the value waits behind that open, not under the row').not.toContain('a.rs');
    expect(
      [...body.matchAll(/class="raw"/g)],
      'one disclosure, for the one structured value - the string draws none',
    ).toHaveLength(1);
  });

  it('carries a level value behind its own disclosure', () => {
    const body = drawn({
      answer: {
        kind: 'score',
        score: 1.79,
        levels: [
          { name: 'Routine', value: 0.08, raw: null },
          { name: 'Soon', value: 0.31, raw: { label: 'Soon', scope: 'worth doing this week' } },
          { name: 'structured value', value: 0.61, raw: { urgency: 'high' } },
        ],
        confidence: null,
      },
    });
    expect(
      [...body.matchAll(/class="raw"/g)],
      'one disclosure per structured level, none for the string one',
    ).toHaveLength(2);
  });

  it('labels a disclosure by what it holds, singular and plural', () => {
    const body = drawn({
      answer: {
        kind: 'choice',
        choice: 'billing',
        probabilities: [
          { name: 'billing', value: 0.7 },
          { name: 'infra', value: 0.3 },
        ],
        confidence: null,
      },
      criteria: {
        billing: {
          text: 'the ledger',
          raw: { label: 'the ledger', files: ['a.rs', 'ledger.rs'] },
        },
        infra: { text: 'deploy keys', raw: { owns: 'deploy keys' } },
      },
    });

    expect(body, 'two fields').toContain('structured value - 2 fields');
    // The closing tag terminates the count: `- 1 field` is a prefix of
    // `- 1 fields`, so an unterminated assertion cannot tell the two apart.
    expect(body, 'one field, singular').toContain('structured value - 1 field</summary>');
    expect(
      drawn({ question: { text: 'structured instructions', raw: ['a.rs', 'b.rs'] } }),
      'an array counts items',
    ).toContain('structured instructions - 2 items');
    expect(
      drawn({ question: { text: 'structured instructions', raw: ['a.rs'] } }),
      'one item, singular',
    ).toContain('structured instructions - 1 item</summary>');
  });

  it("composes the same disclosure the book's structured specimen draws", () => {
    // The same tie the facts line keeps, and tighter: the page's specimen must
    // be what the block would print, the pretty JSON included.
    const value = {
      label: 'owns the ledger and the settlement path',
      files: ['a.rs', 'ledger.rs'],
    };
    const body = drawn({
      answer: {
        kind: 'choice',
        choice: 'billing',
        probabilities: [
          { name: 'billing', value: 0.84 },
          { name: 'frontend', value: 0.16 },
        ],
        confidence: null,
      },
      criteria: {
        billing: { text: 'owns the ledger and the settlement path', raw: value },
      },
    });

    const printed = JSON.stringify(value, null, 2);
    expect(body, 'the block composes the disclosure label').toContain(
      'structured value - 2 fields',
    );
    // The block's own body, with the markup's escaping undone: the page can
    // only draw the printed form if the block prints it.
    const drawnText = body
      .replaceAll('&quot;', '"')
      .replaceAll('&amp;', '&')
      .replaceAll('&lt;', '<')
      .replaceAll('&gt;', '>');
    // The printed form waits behind the disclosure's open now; what the block
    // composes closed is the label, and `Decision.mount` reads the open.
    expect(drawnText, 'the block prints the pretty form behind the open').not.toContain(printed);
    expect(PAGE, 'and the page draws it word for word').toContain('structured value - 2 fields');
    expect(PAGE, 'and the specimen is exactly what the block prints').toContain(printed);
  });
});
