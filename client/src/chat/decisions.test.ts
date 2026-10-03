import { describe, expect, it } from 'vitest';

import { decisionOf } from './decisions';

/** A tool result as the wire delivers one: text parts, and whether it failed. */
function result(text: string, is_error = false) {
  return { content: [{ type: 'text', text }], is_error };
}

/** The noul, choice and score inputs a call carries, as the tools send them. */
const NOUL_INPUT = { state: 'a one-line import fix', instructions: 'Is this mechanical?' };
const SCORE_INPUT = {
  state: 'a ticket',
  instructions: 'How urgent?',
  criteria: ['Routine', 'Soon', 'Urgent'],
};

describe('an answer the client could not read', () => {
  it('falls back to the raw text for anything that is not a decision result', () => {
    const name = 'mcp__forge__systemone__ask_noul';
    // A payload that PARSES, so each fallback is its guard's doing and not a
    // malformed body every return path would refuse anyway.
    const wellFormed = '{"model":"jev-1.13.0","answer":{"type":"noul","noul":0.93}}';

    expect(decisionOf(name, NOUL_INPUT, undefined), 'no result yet').toBeNull();
    expect(decisionOf(name, NOUL_INPUT, result(wellFormed, true)), 'a failed call').toBeNull();
    expect(decisionOf(name, NOUL_INPUT, result('not json at all')), 'malformed').toBeNull();
    expect(
      decisionOf(name, NOUL_INPUT, result('{"model":"m","answer":{"type":"verdict","noul":0.9}}')),
      'an answer shape nothing here draws',
    ).toBeNull();
    expect(
      decisionOf('mcp__forge__agents__list', NOUL_INPUT, result(wellFormed)),
      'a call that is not a decision',
    ).toBeNull();
  });
});

describe('reading the three primitives', () => {
  it('reads a noul with its model and its usage', () => {
    // The live shape: a real call's result, one line of JSON.
    const text = JSON.stringify({
      model: 'jev-1.13.0',
      answer: { type: 'noul', noul: 0.93 },
      usage: { input_tokens: 392, output_tokens: 20 },
    });
    expect(decisionOf('mcp__forge__systemone__ask_noul', NOUL_INPUT, result(text))).toEqual({
      model: 'jev-1.13.0',
      usage: { input_tokens: 392, output_tokens: 20, cost: null },
      answer: { kind: 'noul', noul: 0.93 },
    });
  });

  it('refuses a noul that is not a probability', () => {
    // 1e400 parses to Infinity, which JSON.parse accepts and a judgement never
    // is - written into the text as itself, because JSON.stringify would send
    // it back out as null and prove nothing.
    for (const noul of ['1.5', '1e400']) {
      const text = `{"model":"m","answer":{"type":"noul","noul":${noul}}}`;
      expect(
        decisionOf('mcp__forge__systemone__ask_noul', NOUL_INPUT, result(text)),
        noul,
      ).toBeNull();
    }
  });

  it('reads a choice with its distribution and confidence', () => {
    const text = JSON.stringify({
      model: 'jev-1.13.0',
      answer: {
        type: 'choice',
        choice: 'payments',
        probabilities: { frontend: 0.16, payments: 0.84 },
        confidence: 0.72,
      },
    });
    expect(decisionOf('mcp__forge__systemone__ask_choice', {}, result(text))).toEqual({
      model: 'jev-1.13.0',
      usage: null,
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
  });

  it('keeps a choice whose optionals are absent or unreadable, dropping only those', () => {
    const bare = JSON.stringify({ model: 'm', answer: { type: 'choice', choice: 'payments' } });
    expect(
      decisionOf('mcp__forge__systemone__ask_choice', {}, result(bare))?.answer,
      'absent optionals',
    ).toEqual({ kind: 'choice', choice: 'payments', probabilities: null, confidence: null });

    const drifted = JSON.stringify({
      model: 'm',
      answer: {
        type: 'choice',
        choice: 'payments',
        probabilities: { payments: 0.84, frontend: 'many' },
      },
    });
    expect(
      decisionOf('mcp__forge__systemone__ask_choice', {}, result(drifted))?.answer,
      'a distribution with a non-number member',
    ).toEqual({ kind: 'choice', choice: 'payments', probabilities: null, confidence: null });
  });

  it('refuses a choice with no option to pick', () => {
    const text = JSON.stringify({ model: 'm', answer: { type: 'choice', choice: '' } });
    expect(decisionOf('mcp__forge__systemone__ask_choice', {}, result(text))).toBeNull();
  });

  it('pairs a score level with each criterion, by index', () => {
    const text = JSON.stringify({
      model: 'm',
      answer: { type: 'score', score: 1.79, probabilities: { 0: 0.08, 1: 0.31, 2: 0.61 } },
    });
    expect(
      decisionOf('mcp__forge__systemone__ask_score', SCORE_INPUT, result(text))?.answer,
    ).toEqual({
      kind: 'score',
      score: 1.79,
      levels: [
        { name: 'Routine', value: 0.08 },
        { name: 'Soon', value: 0.31 },
        { name: 'Urgent', value: 0.61 },
      ],
      confidence: null,
    });

    const partial = JSON.stringify({
      model: 'm',
      answer: { type: 'score', score: 1.5, probabilities: { 0: 0.5 } },
    });
    expect(
      decisionOf('mcp__forge__systemone__ask_score', SCORE_INPUT, result(partial))?.answer,
      'a level with no probability keeps its name',
    ).toEqual({
      kind: 'score',
      score: 1.5,
      levels: [
        { name: 'Routine', value: 0.5 },
        { name: 'Soon', value: null },
        { name: 'Urgent', value: null },
      ],
      confidence: null,
    });

    const bare = JSON.stringify({ model: 'm', answer: { type: 'score', score: 1.79 } });
    expect(
      decisionOf('mcp__forge__systemone__ask_score', SCORE_INPUT, result(bare))?.answer,
      'no distribution at all',
    ).toEqual({ kind: 'score', score: 1.79, levels: null, confidence: null });
  });

  it('drops an out-of-range confidence alone', () => {
    const text = JSON.stringify({
      model: 'm',
      answer: { type: 'choice', choice: 'payments', confidence: 1.2 },
    });
    expect(decisionOf('mcp__forge__systemone__ask_choice', {}, result(text))?.answer).toEqual({
      kind: 'choice',
      choice: 'payments',
      probabilities: null,
      confidence: null,
    });
  });

  it('reads a usage that carries a cost', () => {
    // OpenRouter reports a cost; the direct providers do not.
    const text = JSON.stringify({
      model: 'typesafe/jev-1.13-20260917',
      answer: { type: 'noul', noul: 0.98 },
      usage: { input_tokens: 280, output_tokens: 20, cost: 0.00001176 },
    });
    expect(decisionOf('mcp__forge__systemone__ask_noul', NOUL_INPUT, result(text))?.usage).toEqual({
      input_tokens: 280,
      output_tokens: 20,
      cost: 0.00001176,
    });
  });

  it('reads a result whose content is a plain string', () => {
    const text = JSON.stringify({ model: 'm', answer: { type: 'noul', noul: 0.4 } });
    expect(
      decisionOf('mcp__forge__systemone__ask_noul', NOUL_INPUT, { content: text })?.answer,
    ).toEqual({
      kind: 'noul',
      noul: 0.4,
    });
  });
});
