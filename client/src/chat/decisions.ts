/**
 * A System One decision, read off the call's own result.
 *
 * The result text is one line of JSON (`{"model", "answer", "usage"?}`), and
 * this module turns it into the shape the view draws. `null` is the
 * raw-text fallback every other call gets - a result this cannot read is
 * never dropped, only left undressed as a decision.
 */

import { isDecisionTool } from './families';

/**
 * One row's words, and the value in full when it was not a string.
 *
 * A structured value is named by its own naming field where it has one, draws
 * as its fallback where it has none, and is never dropped: `raw` carries the
 * value whole for the row's disclosure (Ved, 2026-10-06).
 */
export interface Row {
  text: string;
  /** The value whole, for the row's disclosure; null where it was a string. */
  raw: unknown;
}

/** What answered, what it cost, and the typed answer. */
export interface Decision {
  model: string;
  usage: { input_tokens: number; output_tokens: number; cost: number | null } | null;
  answer: DecisionAnswer;
  /**
   * The question the call asked, whole - the row's title holds one line of it.
   *
   * The input's `instructions`, which is what the model was asked and the one
   * thing a reader qualifying the answer needs (Ved, 2026-10-04); a structured
   * one draws its naming field, its value behind a disclosure.
   */
  question: Row | null;
  /**
   * The descriptions the options were given, by the name the block's rows use.
   *
   * A noul's two sides land under `yes` and `no`, the words its rows draw; a
   * choice maps option name to its description, and an option whose
   * description was null stands alone; a score's levels ARE their
   * descriptions, so its map is empty.
   */
  criteria: Record<string, Row>;
}

/** One typed answer, as the three tools return them. */
export type DecisionAnswer =
  | { kind: 'noul'; noul: number }
  | {
      kind: 'choice';
      choice: string;
      probabilities: { name: string; value: number }[] | null;
      confidence: number | null;
    }
  | {
      kind: 'score';
      score: number;
      /** One row per criterion in the call's order, or null where absent. */
      levels: Level[] | null;
      confidence: number | null;
    };

/** One score level: the words its row draws, its probability, its value. */
export interface Level {
  name: string;
  value: number | null;
  /** The value whole, for the row's disclosure; null where it was a string. */
  raw: unknown;
}

/** The word a decision call's row leads with, by tool. */
const DECISION_WORDS: Readonly<Record<string, string>> = {
  mcp__forge__systemone__ask_noul: 'ask noul',
  mcp__forge__systemone__ask_choice: 'ask choice',
  mcp__forge__systemone__ask_score: 'ask score',
};

/** What the tool is called on its own row, or null for every other call. */
export function decisionWord(name: string): string | null {
  return DECISION_WORDS[name] ?? null;
}

/**
 * One decision, or null for every result this module will not dress.
 *
 * The result is authoritative: the arm is picked by the answer's own `type`,
 * not by which tool was called, so a mismatched pair still draws what came
 * back. A required field that does not read makes the whole thing null; an
 * optional one that does not read drops alone.
 */
export function decisionOf(
  name: string,
  input: unknown,
  result: { content?: unknown; is_error?: unknown } | undefined,
): Decision | null {
  if (!isDecisionTool(name)) return null;
  if (result === undefined || result.is_error === true) return null;
  const json = obj(parsedText(result.content));
  const answer = answerOf(json['answer'], input);
  if (answer === null) return null;
  return {
    model: str(json, 'model') ?? '',
    usage: usageOf(json['usage']),
    answer,
    question: instructionsOf(input),
    criteria: criteriaOf(answer, input),
  };
}

/** The fields that can name a row, in the order the block tries them. */
const NAMING_FIELDS = ['label', 'question', 'name', 'text'] as const;

/** What a value calls itself, or null where no naming field says. */
function namingField(value: Record<string, unknown>): string | null {
  for (const field of NAMING_FIELDS) {
    const said = str(value, field)?.trim() ?? '';
    if (said !== '') return said;
  }
  return null;
}

/**
 * One row read off one value. A string is its own text; `null` draws nothing;
 * anything else is named by its own naming field where it has one, draws as
 * `fallback` where it has none, and keeps the value whole for the disclosure.
 */
function rowOf(value: unknown, fallback: string): Row | null {
  if (value === null || value === undefined) return null;
  if (typeof value === 'string') {
    const said = value.trim();
    return said === '' ? null : { text: said, raw: null };
  }
  if (typeof value === 'object') {
    return { text: namingField(value as Record<string, unknown>) ?? fallback, raw: value };
  }
  return { text: fallback, raw: value };
}

/** The question the call asked, whole, or null when it said nothing. */
function instructionsOf(input: unknown): Row | null {
  return rowOf(obj(input)['instructions'], 'structured instructions');
}

/** The per-option descriptions, under the names the block's rows draw. */
function criteriaOf(answer: DecisionAnswer, input: unknown): Record<string, Row> {
  const held = obj(obj(input)['criteria']);
  // A null prototype, so a level or option named `constructor` reads as the
  // absent description it is rather than through Object.prototype (the 1723
  // review measured `function Object() { [native code] }` drawing as one).
  const out: Record<string, Row> = Object.create(null) as Record<string, Row>;
  if (answer.kind === 'noul') {
    for (const [key, name] of [
      ['true', 'yes'],
      ['false', 'no'],
    ] as const) {
      const row = rowOf(held[key], 'structured value');
      if (row !== null) out[name] = row;
    }
    return out;
  }
  if (answer.kind === 'choice') {
    for (const [name, value] of Object.entries(held)) {
      const row = rowOf(value, 'structured value');
      if (row !== null) out[name] = row;
    }
  }
  return out;
}

/** A value as a JSON object, or an empty one for every other shape. */
function obj(value: unknown): Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : {};
}

/** One field of a JSON object, when it is a string. */
function str(value: Record<string, unknown>, key: string): string | null {
  const held = value[key];
  return typeof held === 'string' ? held : null;
}

/** A finite number, or null for anything else - Infinity included. */
function finite(value: unknown): number | null {
  return typeof value === 'number' && Number.isFinite(value) ? value : null;
}

/** A 0..=1 field, which is what a noul and a confidence both are. */
function unitOrNull(value: unknown): number | null {
  const number = finite(value);
  return number !== null && number >= 0 && number <= 1 ? number : null;
}

/** The JSON a result's first text part carries, when it is JSON at all. */
function parsedText(content: unknown): unknown {
  const parse = (text: string): unknown => {
    try {
      return JSON.parse(text) as unknown;
    } catch {
      return null;
    }
  };
  if (typeof content === 'string') return parse(content);
  if (!Array.isArray(content)) return null;
  for (const block of content) {
    const held = obj(block);
    if (held['type'] === 'text' && typeof held['text'] === 'string') return parse(held['text']);
  }
  return null;
}

/** The answer, its arm picked by the result's own `type`. */
function answerOf(value: unknown, input: unknown): DecisionAnswer | null {
  const answer = obj(value);
  const type = answer['type'];
  if (type === 'noul') {
    const noul = unitOrNull(answer['noul']);
    return noul === null ? null : { kind: 'noul', noul };
  }
  if (type === 'choice') {
    const choice = str(answer, 'choice');
    if (choice === null || choice.trim() === '') return null;
    return {
      kind: 'choice',
      choice,
      probabilities: probabilitiesOf(answer['probabilities']),
      confidence: unitOrNull(answer['confidence']),
    };
  }
  if (type === 'score') {
    const score = finite(answer['score']);
    if (score === null) return null;
    return {
      kind: 'score',
      score,
      levels: levelsOf(input, answer['probabilities']),
      confidence: unitOrNull(answer['confidence']),
    };
  }
  return null;
}

/**
 * The distribution as rows in the result's own key order, or null where it
 * is absent or carries anything that is not a number.
 */
function probabilitiesOf(value: unknown): { name: string; value: number }[] | null {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return null;
  const rows: { name: string; value: number }[] = [];
  for (const [name, held] of Object.entries(value)) {
    const number = finite(held);
    if (number === null) return null;
    rows.push({ name, value: number });
  }
  return rows.length === 0 ? null : rows;
}

/**
 * A score's levels: the call's criteria, each paired with the probability the
 * result gave for its index. A level the result skipped keeps its name and a
 * null value; a structured level keeps its own words and value rather than
 * taking the whole distribution with it; no criteria or no distribution at
 * all draws no levels.
 */
function levelsOf(input: unknown, value: unknown): Level[] | null {
  const criteria = obj(input)['criteria'];
  if (!Array.isArray(criteria) || criteria.length === 0) return null;
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return null;
  const probabilities = value as Record<string, unknown>;
  const levels: Level[] = [];
  for (const [at, held] of criteria.entries()) {
    const row = rowOf(held, 'structured value');
    levels.push({
      name: row?.text ?? String(at),
      value: finite(probabilities[String(at)]),
      raw: row?.raw ?? null,
    });
  }
  return levels;
}

/** The usage the footnote draws, or null for a result that carried none. */
function usageOf(value: unknown): Decision['usage'] {
  const held = obj(value);
  const input_tokens = finite(held['input_tokens']);
  const output_tokens = finite(held['output_tokens']);
  if (input_tokens === null || output_tokens === null) return null;
  return { input_tokens, output_tokens, cost: finite(held['cost']) };
}
