import { render } from 'svelte/server';
import { describe, expect, it } from 'vitest';

import Decision from './Decision.svelte';
import type { Decision as Parsed } from './decisions';

/** One decision, as the fold hands it to the block. */
const decision = (over: Partial<Parsed>): Parsed => ({
  model: 'jev-1.13.0',
  usage: { input_tokens: 392, output_tokens: 20, cost: null },
  answer: { kind: 'noul', noul: 0.93 },
  ...over,
});

/** The block, as the reader sees it. */
const drawn = (over: Partial<Parsed>): string =>
  render(Decision, { props: { decision: decision(over) } }).body;

describe('the block one decision draws', () => {
  it('draws a noul as its number, its word, and both sides of the yes/no', () => {
    const body = drawn({});
    expect(body, 'the number as the result returned it').toContain('>0.93<');
    // The verdict's own span, not `>yes<`: the distribution's yes row draws
    // the same word, so the loose form survives a swapped verdict word.
    expect(body, 'the word the majority side reads as').toContain('class="read">yes</span>');
    expect(body, 'and the other side, derived, as a value of its own').toContain('>0.07<');
    expect(body, 'the number carries the sure tone').toContain('num sure');
    expect(body, 'and the side that won carries the mark').toContain('opt win');
    expect(body, 'the derived side fills at the rounded grain').toContain('width:7%');
  });

  it('tones a coin flip without changing the words', () => {
    const body = drawn({ answer: { kind: 'noul', noul: 0.52 } });
    expect(body, 'the number draws unsure').toContain('num unsure');
    expect(body, 'and the word is still the majority side').toContain('class="read">yes</span>');
  });

  it('draws a choice as its pick and its distribution', () => {
    const body = drawn({
      answer: {
        kind: 'choice',
        choice: 'payments',
        probabilities: [
          { name: 'frontend', value: 0.16 },
          { name: 'payments', value: 0.84 },
        ],
        confidence: 0.72,
      },
    });
    expect(body, 'the pick, emphasised').toContain('<b>payments</b>');
    expect(body, 'with its confidence beside it').toContain('confidence 0.72');
    expect(body, 'every option draws its value as text').toContain('>0.16<');
    expect([...body.matchAll(/opt win/g)], 'and only the pick carries the mark').toHaveLength(1);
  });

  it('draws a bare pick with no distribution and no confidence', () => {
    const body = drawn({
      answer: { kind: 'choice', choice: 'payments', probabilities: null, confidence: null },
    });
    expect(body, 'the pick still draws').toContain('<b>payments</b>');
    expect(body, 'no distribution table').not.toContain('class="dist"');
    expect(body, 'and no confidence clause').not.toContain('confidence');
  });

  it('draws a score between its levels, both carrying the mark', () => {
    const body = drawn({
      answer: {
        kind: 'score',
        score: 1.79,
        levels: [
          { name: 'Routine', value: 0.08 },
          { name: 'Soon', value: 0.31 },
          { name: 'Urgent', value: 0.61 },
        ],
        confidence: null,
      },
    });
    expect(body, 'the number as returned').toContain('1.79');
    expect(body, 'of the last level index, between the levels it sits in').toContain(
      'of 2 - between Soon and Urgent',
    );
    expect(
      [...body.matchAll(/opt win/g)],
      'the two levels it sits between carry the mark',
    ).toHaveLength(2);
  });

  it('draws a level the result left without a probability, which the verdict still names', () => {
    // One row per level: skipping the value-less one made the verdict say
    // "between Soon and Urgent" while Soon drew nowhere.
    const body = drawn({
      answer: {
        kind: 'score',
        score: 1.5,
        levels: [
          { name: 'Routine', value: 0.5 },
          { name: 'Soon', value: null },
          { name: 'Urgent', value: 0.4 },
        ],
        confidence: null,
      },
    });
    expect(body, 'the level still draws its row').toContain('>Soon<');
    expect([...body.matchAll(/class="opt/g)], 'one row per level').toHaveLength(3);
    expect(body, 'with no value text where none was reported').not.toContain('>null<');
  });

  it('draws a score with no levels as its number alone', () => {
    const body = drawn({ answer: { kind: 'score', score: 1.79, levels: null, confidence: null } });
    expect(body).toContain('1.79');
    expect(body, 'no level words').not.toContain('of 2');
    expect(body, 'and no distribution').not.toContain('class="dist"');
  });

  it('draws the model, the tokens and the cost when the provider reported one', () => {
    const body = drawn({ usage: { input_tokens: 280, output_tokens: 20, cost: 0.00001176 } });
    expect(body, 'the model').toContain('jev-1.13.0');
    expect(body, 'the tokens').toContain('280 in - 20 out');
    expect(body, 'and the cost').toContain('$0.00001176');
    expect(
      drawn({ usage: { input_tokens: 280, output_tokens: 20, cost: null } }),
      'no cost segment where the provider reported none',
    ).not.toContain('$');

    expect(drawn({ usage: null }), 'no usage reported, no footer drawn').not.toContain(
      'class="foot"',
    );
  });

  it('leaves no separator behind for a result that named no model', () => {
    const body = drawn({
      model: '',
      usage: { input_tokens: 280, output_tokens: 20, cost: 0.00001176 },
    });
    expect(
      [...body.matchAll(/class="sep"/g)],
      'one separator, between the tokens and the cost',
    ).toHaveLength(1);
  });
});
