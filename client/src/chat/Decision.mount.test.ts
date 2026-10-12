// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it } from 'vitest';

import Decision from './Decision.svelte';
import type { Decision as Parsed } from './decisions';

/**
 * The block under the runtime renderer, for the things SSR cannot hold.
 *
 * The keyed list's duplicate check is the runtime's: a server render draws
 * duplicate keys happily, so a case like the repeated level names can only be
 * pinned by mounting.
 */
let app: Record<string, unknown> | null = null;

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  document.body.innerHTML = '';
});

const decision = (over: Partial<Parsed> = {}): Parsed => ({
  model: 'jev-1.13.0',
  usage: null,
  answer: {
    kind: 'score',
    score: 1.5,
    levels: [
      { name: 'Soon', value: 0.3, raw: null },
      { name: 'Soon', value: 0.4, raw: null },
      { name: 'Urgent', value: 0.3, raw: null },
    ],
    confidence: null,
  },
  question: null,
  criteria: {},
  state: null,
  chips: [{ text: 'score 1.5 of 2', tone: 'plain' }],
  figure: null,
  ...over,
});

describe('the distribution the runtime draws', () => {
  it('draws levels whose names repeat, which the server does not forbid', () => {
    // The server bounds a criteria list's COUNT only, so two levels may share
    // a name; naming the rows after them throws `each_key_duplicate` here and
    // takes the whole lane down with it.
    app = mount(Decision, { target: document.body, props: { decision: decision() } });
    flushSync();

    expect(document.querySelectorAll('.fam-opt')).toHaveLength(3);
  });

  it('draws a structured value onto its own open, question and criterion alike', () => {
    const value = { label: 'owns the ledger and the settlement path', files: ['a.rs'] };
    const parsed: Parsed = decision({
      answer: {
        kind: 'choice',
        choice: 'billing',
        probabilities: [{ name: 'billing', value: 0.84 }],
        confidence: null,
      },
      question: {
        text: 'Is the claim `just check` green?',
        raw: { evidence: { verdict: 'all green' } },
      },
      criteria: {
        billing: { text: 'owns the ledger and the settlement path', raw: value },
      },
    });

    app = mount(Decision, { target: document.body, props: { decision: parsed } });
    flushSync();
    expect(document.body.textContent ?? '', 'closed, the value waits').not.toContain('all green');

    for (const raw of document.querySelectorAll('details.raw')) {
      if (raw instanceof HTMLDetailsElement) {
        raw.open = true;
        raw.dispatchEvent(new Event('toggle'));
      }
    }
    flushSync();

    expect(document.body.textContent ?? '', 'the question raw draws onto its open').toContain(
      'all green',
    );
    expect(document.body.textContent ?? '', 'and the criterion raw').toContain('a.rs');
    // The PRETTY form, not a plain stringify: the page's specimen draws the
    // indented shape, and a body that printed the compact one would still
    // carry both words above.
    expect(document.body.textContent ?? '', 'the pretty form, indentation and all').toContain(
      JSON.stringify(value, null, 2),
    );
  });

  it('draws the state onto its own open, by the shape it arrived in', () => {
    const parsed: Parsed = decision({
      state: {
        kind: 'lines',
        lines: [
          { key: 'claim', text: 'the PR is ready', depth: 0, dim: false },
          { key: 'evidence', text: '2 fields', depth: 0, dim: true },
          { key: 'ci', text: 'green', depth: 1, dim: false },
        ],
        size: '2 fields',
      },
    });

    app = mount(Decision, { target: document.body, props: { decision: parsed } });
    flushSync();
    expect(document.body.textContent ?? '', 'closed, the material waits').not.toContain(
      'the PR is ready',
    );

    const fold = document.querySelector('details.fold');
    if (fold instanceof HTMLDetailsElement) {
      fold.open = true;
      fold.dispatchEvent(new Event('toggle'));
    }
    flushSync();

    expect(document.body.textContent ?? '', 'the fold line names its size').toContain(
      'state - 2 fields',
    );
    expect(document.body.textContent ?? '', 'and the material draws under it').toContain(
      'the PR is ready',
    );
    expect(
      document.querySelectorAll('.st-row').length,
      'one row per line the read handed over',
    ).toBe(3);
  });

  it('draws a string state as prose under the same fold', () => {
    const parsed: Parsed = decision({
      state: { kind: 'prose', text: 'the ticket moves to payments', size: '28 chars' },
    });

    app = mount(Decision, { target: document.body, props: { decision: parsed } });
    flushSync();

    const fold = document.querySelector('details.fold');
    if (fold instanceof HTMLDetailsElement) {
      fold.open = true;
      fold.dispatchEvent(new Event('toggle'));
    }
    flushSync();

    const quote = document.querySelector('.st-str');
    expect(quote?.textContent ?? '', 'the material as prose, under its own class').toBe(
      'the ticket moves to payments',
    );
  });
});
