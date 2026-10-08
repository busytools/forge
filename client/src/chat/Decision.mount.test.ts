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

const decision = (levels: { name: string; value: number | null }[]): Parsed => ({
  model: 'jev-1.13.0',
  usage: null,
  answer: {
    kind: 'score',
    score: 1.5,
    levels: levels.map((level) => ({ ...level, raw: null })),
    confidence: null,
  },
  question: null,
  criteria: {},
});

describe('the distribution the runtime draws', () => {
  it('draws levels whose names repeat, which the server does not forbid', () => {
    // The server bounds a criteria list's COUNT only, so two levels may share
    // a name; naming the rows after them throws `each_key_duplicate` here and
    // takes the whole lane down with it.
    app = mount(Decision, {
      target: document.body,
      props: {
        decision: decision([
          { name: 'Soon', value: 0.3 },
          { name: 'Soon', value: 0.4 },
          { name: 'Urgent', value: 0.3 },
        ]),
      },
    });
    flushSync();

    expect(document.querySelectorAll('.opt')).toHaveLength(3);
  });

  it('draws a structured value onto its own open, question and criterion alike', () => {
    const value = { label: 'owns the ledger and the settlement path', files: ['a.rs'] };
    const parsed: Parsed = {
      model: 'jev-1.13.0',
      usage: null,
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
    };

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
  });
});
