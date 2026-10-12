import { describe, expect, it } from 'vitest';

import { decisionOf, decisionTitle } from './decisions';

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
      question: { text: 'Is this mechanical?', raw: null },
      criteria: {},
      state: { kind: 'prose', text: 'a one-line import fix', size: '21 chars' },
      chips: [{ text: 'yes', tone: 'ok' }],
      figure: '0.93',
    });
  });

  it('keeps the whole question and the options the call was made with', () => {
    // The row's title cuts the question to its first line, and the block drew
    // the answer alone - so nothing on the page held the rest of it, or what
    // the options meant (Ved, 2026-10-04: "I'm not able to see the question,
    // the full question, even after I expand - and also full options").
    const text = JSON.stringify({ model: 'm', answer: { type: 'noul', noul: 0.93 } });
    const noul = decisionOf(
      'mcp__forge__systemone__ask_noul',
      {
        state: 'a one-line import fix',
        instructions: 'Is this mechanical?\nOr does it touch the shared module?',
        criteria: { true: 'a single import line', false: 'anything else moves' },
      },
      result(text),
    );
    expect(noul?.question, 'the question whole, not its first line').toEqual({
      text: 'Is this mechanical?\nOr does it touch the shared module?',
      raw: null,
    });
    expect(noul?.criteria, "and what yes and no meant, under the rows' own words").toEqual({
      yes: { text: 'a single import line', raw: null },
      no: { text: 'anything else moves', raw: null },
    });

    const choiceText = JSON.stringify({
      model: 'm',
      answer: { type: 'choice', choice: 'payments', probabilities: { payments: 0.84 } },
    });
    const choice = decisionOf(
      'mcp__forge__systemone__ask_choice',
      { instructions: 'Which team?', criteria: { payments: 'owns the ledger', frontend: null } },
      result(choiceText),
    );
    expect(choice?.question).toEqual({ text: 'Which team?', raw: null });
    expect(choice?.criteria, 'a null description stands alone rather than drawing').toEqual({
      payments: { text: 'owns the ledger', raw: null },
    });

    // A score's levels ARE their descriptions, so nothing sits beside them.
    const scoreText = JSON.stringify({
      model: 'm',
      answer: { type: 'score', score: 1.0, probabilities: { 0: 0.5, 1: 0.5 } },
    });
    const score = decisionOf('mcp__forge__systemone__ask_score', SCORE_INPUT, result(scoreText));
    expect(score?.question).toEqual({ text: 'How urgent?', raw: null });
    expect(score?.criteria).toEqual({});
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
      question: null,
      criteria: {},
      state: null,
      chips: [{ text: 'pick: payments', tone: 'info' }],
      figure: '0.84',
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
        { name: 'Routine', value: 0.08, raw: null },
        { name: 'Soon', value: 0.31, raw: null },
        { name: 'Urgent', value: 0.61, raw: null },
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
        { name: 'Routine', value: 0.5, raw: null },
        { name: 'Soon', value: null, raw: null },
        { name: 'Urgent', value: null, raw: null },
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

describe('a structured value, read the way the block draws it', () => {
  // The tools take the API's own union for `instructions` and criteria values
  // (#1771), and this block is the only surface that reads them back. The
  // picked shape (Ved, 2026-10-06): a naming field names the row, the value in
  // full rides for the disclosure, and nothing is dropped.

  it('names its row from the naming field and keeps the value for the disclosure', () => {
    const text = JSON.stringify({ model: 'm', answer: { type: 'noul', noul: 0.93 } });
    const noul = decisionOf(
      'mcp__forge__systemone__ask_noul',
      {
        state: 'a one-line import fix',
        instructions: {
          question: 'Is the claim `just check` green?',
          evidence: { verdict: 'all green' },
        },
        criteria: { true: { label: 'the verdict line says all green' }, false: 'anything else' },
      },
      result(text),
    );

    expect(noul?.question, 'the naming field as the line, the value whole beside it').toEqual({
      text: 'Is the claim `just check` green?',
      raw: { question: 'Is the claim `just check` green?', evidence: { verdict: 'all green' } },
    });
    expect(
      noul?.criteria,
      'a named side draws as a string row, an unnamed one keeps its value',
    ).toEqual({
      yes: {
        text: 'the verdict line says all green',
        raw: { label: 'the verdict line says all green' },
      },
      no: { text: 'anything else', raw: null },
    });
  });

  it('falls back to the structured words when nothing names the row', () => {
    const text = JSON.stringify({ model: 'm', answer: { type: 'noul', noul: 0.52 } });
    const noul = decisionOf(
      'mcp__forge__systemone__ask_noul',
      {
        state: 'a rename',
        instructions: { 'the ask': 'Should the rename touch the shared module now?' },
        criteria: { true: { rule: 'renaming later is a second migration' }, false: null },
      },
      result(text),
    );

    expect(noul?.question).toEqual({
      text: 'structured instructions',
      raw: { 'the ask': 'Should the rename touch the shared module now?' },
    });
    expect(noul?.criteria, 'the unnamed side still draws; a null side still does not').toEqual({
      yes: {
        text: 'structured value',
        raw: { rule: 'renaming later is a second migration' },
      },
    });
  });

  it('skips a naming field that is not a non-empty string', () => {
    const text = JSON.stringify({ model: 'm', answer: { type: 'choice', choice: 'billing' } });
    const choice = decisionOf(
      'mcp__forge__systemone__ask_choice',
      {
        instructions: 'Which team?',
        criteria: {
          billing: { label: 3, name: '   ', text: 'the ledger and the settlement path' },
          frontend: ['a.rs'],
          infra: { name: 'the deploy keys', text: 'the ordered fallback' },
        },
      },
      result(text),
    );

    expect(choice?.criteria['billing'], 'the first field that can name it wins').toEqual({
      text: 'the ledger and the settlement path',
      raw: { label: 3, name: '   ', text: 'the ledger and the settlement path' },
    });
    expect(choice?.criteria['frontend'], 'an array names nothing and still draws').toEqual({
      text: 'structured value',
      raw: ['a.rs'],
    });
    expect(
      choice?.criteria['infra'],
      'and where several could name it, the order settles it',
    ).toEqual({
      text: 'the deploy keys',
      raw: { name: 'the deploy keys', text: 'the ordered fallback' },
    });
  });

  it('names the row by the first naming field that can, in the sheet order', () => {
    // The head of the order matters: a value carrying both a label and a
    // question is a label first, and the row must read that one.
    const text = JSON.stringify({ model: 'm', answer: { type: 'choice', choice: 'billing' } });
    const choice = decisionOf(
      'mcp__forge__systemone__ask_choice',
      {
        instructions: 'Which team?',
        criteria: {
          billing: { label: 'the label words', question: 'the question words' },
        },
      },
      result(text),
    );

    expect(choice?.criteria['billing'], 'label leads the order, question follows').toEqual({
      text: 'the label words',
      raw: { label: 'the label words', question: 'the question words' },
    });
  });

  it('draws a primitive criterion or level as the fallback rather than dropping it', () => {
    // The API types no number or boolean value, but the tools take any JSON -
    // a caller can send one, and it must not vanish.
    const text = JSON.stringify({ model: 'm', answer: { type: 'choice', choice: 'billing' } });
    const choice = decisionOf(
      'mcp__forge__systemone__ask_choice',
      { instructions: 'Which team?', criteria: { billing: 7, frontend: false } },
      result(text),
    );

    expect(choice?.criteria, 'a number and a boolean both keep their row and their value').toEqual({
      billing: { text: 'structured value', raw: 7 },
      frontend: { text: 'structured value', raw: false },
    });

    const scoreText = JSON.stringify({
      model: 'm',
      answer: { type: 'score', score: 0.5, probabilities: { 0: 0.5, 1: 0.5 } },
    });
    const score = decisionOf(
      'mcp__forge__systemone__ask_score',
      { instructions: 'How urgent?', criteria: [3, 'Urgent'] },
      result(scoreText),
    );
    expect(score?.answer, 'and a level that is a number keeps its row too').toEqual({
      kind: 'score',
      score: 0.5,
      levels: [
        { name: 'structured value', value: 0.5, raw: 3 },
        { name: 'Urgent', value: 0.5, raw: null },
      ],
      confidence: null,
    });
  });

  it('drops an empty or whitespace criterion, which names nothing', () => {
    const text = JSON.stringify({ model: 'm', answer: { type: 'choice', choice: 'billing' } });
    const choice = decisionOf(
      'mcp__forge__systemone__ask_choice',
      {
        instructions: 'Which team?',
        criteria: { billing: '   ', frontend: '', infra: 'the words' },
      },
      result(text),
    );

    expect(choice?.criteria, 'only the value with words draws').toEqual({
      infra: { text: 'the words', raw: null },
    });
  });

  it('keeps the whole distribution when a level is not a string', () => {
    // One structured level used to take every row with it: `levelsOf` refused
    // the criteria array and the block drew a bare number.
    const text = JSON.stringify({
      model: 'm',
      answer: { type: 'score', score: 1.79, probabilities: { 0: 0.08, 1: 0.31, 2: 0.61 } },
    });
    const score = decisionOf(
      'mcp__forge__systemone__ask_score',
      {
        state: 'a ticket',
        instructions: 'How urgent?',
        criteria: [
          'Routine',
          { label: 'Soon', scope: 'worth doing this week' },
          { urgency: 'high' },
        ],
      },
      result(text),
    );

    expect(score?.answer).toEqual({
      kind: 'score',
      score: 1.79,
      levels: [
        { name: 'Routine', value: 0.08, raw: null },
        { name: 'Soon', value: 0.31, raw: { label: 'Soon', scope: 'worth doing this week' } },
        { name: 'structured value', value: 0.61, raw: { urgency: 'high' } },
      ],
      confidence: null,
    });
  });
});

describe('the state the call judged', () => {
  // The state is the material the decision was made on, and it went unread
  // until the picked card: it is the one thing that answers what all was used
  // in the call, and the longest part of most of them (Ved, 2026-10-12).
  const answer = (kind: 'noul' | 'choice'): string =>
    kind === 'noul'
      ? JSON.stringify({ model: 'm', answer: { type: 'noul', noul: 0.6 } })
      : JSON.stringify({
          model: 'm',
          answer: { type: 'choice', choice: 'a', probabilities: { a: 1 } },
        });

  it('reads a string state as prose, with the size the fold line names', () => {
    const decision = decisionOf(
      'mcp__forge__systemone__ask_noul',
      { state: 'the ticket moves to the payments team', instructions: 'Which team?' },
      result(answer('noul')),
    );
    expect(decision?.state).toEqual({
      kind: 'prose',
      text: 'the ticket moves to the payments team',
      size: '37 chars',
    });
  });

  it('reads an object state as its named fields, a nested value as the count of what it holds', () => {
    const state = { claim: 'the PR is ready', evidence: { ci: 'green', review: 'passed' } };
    const decision = decisionOf(
      'mcp__forge__systemone__ask_noul',
      { state, instructions: 'Is it ready?' },
      result(answer('noul')),
    );
    expect(decision?.state).toEqual({
      kind: 'lines',
      lines: [
        { key: 'claim', text: 'the PR is ready', depth: 0, dim: false },
        { key: 'evidence', text: '2 fields', depth: 0, dim: true },
        { key: 'ci', text: 'green', depth: 1, dim: false },
        { key: 'review', text: 'passed', depth: 1, dim: false },
      ],
      size: '2 fields',
    });
  });

  it('reads an array state as its own rows, and a null value as the null it is', () => {
    const decision = decisionOf(
      'mcp__forge__systemone__ask_choice',
      { state: ['a.rs', 'b.rs'], instructions: 'Which file?' },
      result(answer('choice')),
    );
    expect(decision?.state).toEqual({
      kind: 'lines',
      lines: [
        { key: null, text: 'a.rs', depth: 0, dim: false },
        { key: null, text: 'b.rs', depth: 0, dim: false },
      ],
      size: '2 items',
    });

    const withNull = decisionOf(
      'mcp__forge__systemone__ask_choice',
      { state: { claim: null }, instructions: 'Which file?' },
      result(answer('choice')),
    );
    expect(withNull?.state).toEqual({
      kind: 'lines',
      lines: [{ key: 'claim', text: 'null', depth: 0, dim: true }],
      size: '1 field',
    });
  });

  it('draws a primitive state as its own word, which has no size to name', () => {
    // The schema takes any JSON, so a caller can send one; it is drawn rather
    // than dropped like everything else a call carried.
    const decision = decisionOf(
      'mcp__forge__systemone__ask_noul',
      { state: 3, instructions: 'Which team?' },
      result(answer('noul')),
    );
    expect(decision?.state).toEqual({ kind: 'prose', text: '3', size: null });

    const nulled = decisionOf(
      'mcp__forge__systemone__ask_noul',
      { state: null, instructions: 'Which team?' },
      result(answer('noul')),
    );
    expect(nulled?.state, 'a null state is a value the caller sent, not an absent one').toEqual({
      kind: 'prose',
      text: 'null',
      size: null,
    });
  });

  it('carries no state at all where the call sent none', () => {
    const decision = decisionOf(
      'mcp__forge__systemone__ask_noul',
      { instructions: 'Which team?' },
      result(answer('noul')),
    );
    expect(decision?.state).toBeNull();
  });
});

describe("the distribution in the caller's own order", () => {
  it('draws the options in the order the criteria were written, a row they never named after them', () => {
    // The result's own key order is alphabetical and carries nothing; the
    // order the caller wrote is what the model saw and what a reader
    // recognises as the options they gave (Ved, 2026-10-12).
    const text = JSON.stringify({
      model: 'm',
      answer: {
        type: 'choice',
        choice: 'no mid-flow cue',
        probabilities: {
          'spawn-time absence': 0.4,
          'no mid-flow cue': 0.6,
          'no visible benefit': 0,
        },
      },
    });
    const decision = decisionOf(
      'mcp__forge__systemone__ask_choice',
      {
        instructions: 'Which cause?',
        criteria: {
          'no visible benefit': 'nobody saw it help',
          'spawn-time absence': 'never injected',
        },
      },
      result(text),
    );
    expect(decision?.answer).toEqual({
      kind: 'choice',
      choice: 'no mid-flow cue',
      probabilities: [
        { name: 'no visible benefit', value: 0 },
        { name: 'spawn-time absence', value: 0.4 },
        { name: 'no mid-flow cue', value: 0.6 },
      ],
      confidence: null,
    });
  });

  it("keeps a noul's two sides in their own order, which no criteria names", () => {
    const text = JSON.stringify({ model: 'm', answer: { type: 'noul', noul: 0.4 } });
    const decision = decisionOf(
      'mcp__forge__systemone__ask_noul',
      { instructions: 'Is it?', criteria: { false: 'anything else', true: 'a single line' } },
      result(text),
    );
    expect(decision?.answer).toEqual({ kind: 'noul', noul: 0.4 });
  });
});

describe('the answer as the row states it', () => {
  const chipsOf = (name: string, input: unknown, text: string) =>
    decisionOf(name, input, result(text));

  it('chips a noul as the word it reads as, toned by how sure it is, the number as the figure', () => {
    const noul = (value: number) =>
      chipsOf(
        'mcp__forge__systemone__ask_noul',
        { instructions: 'Is it?' },
        JSON.stringify({ model: 'm', answer: { type: 'noul', noul: value } }),
      );
    expect(noul(0.93), 'sure').toMatchObject({
      chips: [{ text: 'yes', tone: 'ok' }],
      figure: '0.93',
    });
    expect(noul(0.72), 'between, which carries no tone').toMatchObject({
      chips: [{ text: 'yes', tone: 'plain' }],
      figure: '0.72',
    });
    expect(noul(0.52), 'a coin flip').toMatchObject({
      chips: [{ text: 'yes', tone: 'warn' }],
      figure: '0.52',
    });
    expect(noul(0.3), 'and the side that lost is still the word it reads as').toMatchObject({
      chips: [{ text: 'no', tone: 'plain' }],
      figure: '0.3',
    });
  });

  it("chips a choice with its pick, the pick's own probability as the figure", () => {
    const text = JSON.stringify({
      model: 'm',
      answer: {
        type: 'choice',
        choice: 'payments',
        probabilities: { frontend: 0.16, payments: 0.84 },
      },
    });
    expect(chipsOf('mcp__forge__systemone__ask_choice', {}, text)).toMatchObject({
      chips: [{ text: 'pick: payments', tone: 'info' }],
      figure: '0.84',
    });

    const bare = JSON.stringify({ model: 'm', answer: { type: 'choice', choice: 'payments' } });
    expect(
      chipsOf('mcp__forge__systemone__ask_choice', {}, bare),
      'a result with no distribution draws no figure',
    ).toMatchObject({ chips: [{ text: 'pick: payments', tone: 'info' }], figure: null });
  });

  it('chips a score with where it landed and its confidence as the figure', () => {
    const text = JSON.stringify({
      model: 'm',
      answer: { type: 'score', score: 2, probabilities: { 0: 0, 1: 0, 2: 1 }, confidence: 1 },
    });
    expect(
      chipsOf('mcp__forge__systemone__ask_score', { criteria: ['low', 'mid', 'high'] }, text),
    ).toMatchObject({
      chips: [{ text: 'score 2 of 2', tone: 'plain' }],
      figure: 'conf 1',
    });

    const noLevels = JSON.stringify({ model: 'm', answer: { type: 'score', score: 2 } });
    expect(
      chipsOf('mcp__forge__systemone__ask_score', {}, noLevels),
      'no levels to be a score of, and no confidence to state',
    ).toMatchObject({ chips: [{ text: 'score 2', tone: 'plain' }], figure: null });
  });
});

describe("the line a decision's row is titled by", () => {
  it("is the question's first line, never the tool's name", () => {
    expect(
      decisionTitle({ instructions: 'Is this mechanical and reversible?\nAnd nothing else.' }),
    ).toBe('Is this mechanical and reversible?');
  });

  it('names a structured question by its own naming field', () => {
    expect(decisionTitle({ instructions: { question: 'Is the claim green?' } })).toBe(
      'Is the claim green?',
    );
  });

  it("is null where the call named nothing, which is the caller's fallback to take", () => {
    expect(decisionTitle({})).toBeNull();
    expect(decisionTitle({ instructions: '   ' })).toBeNull();
    expect(decisionTitle({ instructions: { 'the ask': 'unnamed' } })).toBeNull();
    expect(decisionTitle({ instructions: 3 })).toBeNull();
  });
});
