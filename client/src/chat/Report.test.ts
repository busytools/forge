import { render } from 'svelte/server';
import { describe, expect, it } from 'vitest';

import Report from './Report.svelte';
import type { TurnInfo } from './units';

/** A settled turn with everything the CLI can report. */
const FULL: TurnInfo = {
  running: false,
  failed: false,
  duration_ms: 161_000,
  api_ms: 64_000,
  ended_at_utc: '2026-09-29T10:18:31.000Z',
  model: 'claude-opus-5',
  thinking_tokens: 434,
  input_tokens: 4_231,
  output_tokens: 1_102,
  cache_read_tokens: 108_442,
  cache_written_tokens: 3_180,
  session_cost_usd: 4.82,
};

const draw = (info: TurnInfo): string => render(Report, { props: { info } }).body;

/** The body's facts as a reader sees them: `label` and figure, in order. */
function facts(body: string): string[] {
  const start = body.indexOf('<div class="tibody">');
  const end = body.indexOf('</details>', start);
  return [...body.slice(start, end).matchAll(/<b>([^<]+)<\/b>\s*<span>([^<]*)<\/span>/g)].map(
    (match) => `${match[1]}=${match[2]}`,
  );
}

describe('a settled turn\u2019s row', () => {
  it('draws a clock, a local span and a cache share when the record carries them', () => {
    const drawn = facts(draw(FULL));

    // The clock is the reader's own zone, so only its shape is asserted.
    expect(drawn[0], 'the instant the turn ended, in the reader\u2019s zone').toMatch(
      /^ended=\d\d:\d\d:\d\d$/,
    );
    expect(drawn, 'what the turn spent on its own tools and hooks').toContain(
      'local=1m 37s tools + hooks',
    );
    // 108,442 read over 108,442 + 4,231 + 3,180, floored: the share is over
    // every input-side counter, not over the bare input alone.
    expect(drawn, 'the share of its input served from the cache').toContain('cached=93% of input');
  });

  it('holds an absent field with a dash rather than a zero', () => {
    const nothing: TurnInfo = {
      running: false,
      failed: false,
      duration_ms: null,
      api_ms: null,
      ended_at_utc: null,
      model: null,
      thinking_tokens: null,
      input_tokens: null,
      output_tokens: null,
      cache_read_tokens: null,
      cache_written_tokens: null,
      session_cost_usd: null,
    };
    const drawn = facts(draw(nothing));

    // A zero reads as a measurement, and the CLI attributing nothing is not
    // one: a compaction result arrives with every counter at zero.
    expect(drawn).toContain('ended=-');
    expect(drawn).toContain('model=-');
    expect(drawn).toContain('in=-');
    // A share of nothing is not a share, so the fact is dropped rather than
    // drawn as a dash.
    expect(drawn.some((fact) => fact.startsWith('cached='))).toBe(false);
  });

  it('draws the running-only segments on no settled row', () => {
    // The thinking count and the cost dash are the running row's shapes: a
    // settled record draws neither the count (its body holds it) nor a cost
    // segment when it has no numeric cost. Each arm is one mutation away from
    // drawing on every row, so each gets its own assertion.
    const summary = (info: TurnInfo): string => {
      const body = draw(info);
      return body.slice(body.indexOf('<summary'), body.indexOf('</summary>'));
    };

    expect(
      summary({ ...FULL, session_cost_usd: null }),
      'no cost segment on a settled row that has none',
    ).not.toContain('cumulative');
    expect(summary(FULL), 'and no thinking count where the row is settled').not.toContain(
      'thinking',
    );
  });

  it('drops an unattributed usage block rather than printing its zeroes', () => {
    const compaction: TurnInfo = {
      ...FULL,
      input_tokens: 0,
      output_tokens: 0,
      cache_read_tokens: 0,
      cache_written_tokens: 0,
    };
    const drawn = facts(draw(compaction));

    expect(drawn, 'the counters vanish together').toContain('in=-');
    expect(drawn).toContain('out=-');
    expect(drawn.some((fact) => fact === 'in=0')).toBe(false);
  });

  it('draws the running row what the frames carry, and the settle-only cost absent', () => {
    // While the turn runs the row leads with a ring, shows the figures the
    // frames already carry, and draws the one settle-only figure as a dash
    // rather than leaving the slot out - an empty gap reads as a field this
    // row does not have. The record keeps FULL's end stamp on purpose: the
    // `ended` fact must dash on the RUNNING arm, not because the stamp is
    // absent.
    const running: TurnInfo = {
      ...FULL,
      running: true,
      api_ms: null,
      session_cost_usd: null,
    };
    const body = draw(running);
    const summary = body.slice(body.indexOf('<summary'), body.indexOf('</summary>'));

    expect(body, 'the ring rather than a settled check').toContain('class="ring"');
    expect(summary, 'the thinking count, which only a running row leads with').toContain(
      'thinking 434',
    );
    expect(summary, 'the token side the frames carry').toContain('4.2k\u{2191}');
    expect(summary, 'and the cost drawn absent rather than dropped').toContain('- cumulative');
    expect(summary, 'claiming no settled figure it has not been given').not.toContain('$');
    expect(facts(body), 'nor an end it does not have').toContain('ended=-');
  });

  it('carries the toggle word as text rather than as a stylesheet rule', () => {
    // A `::after` label is generated content, so the disclosure's accessible
    // name is whatever the user agent makes of it.
    expect(draw(FULL)).toContain('>expand<');
  });

  it('marks the row from the sprite, not from a character cell', () => {
    const body = draw(FULL);

    expect(body, 'the settled mark is an icon every other row also uses').toContain('i-check');
    expect(body, 'and not the arrow a terminal drew').not.toContain('\u{21A9}');
  });

  it('marks a turn that failed with the failure mark, not the check', () => {
    // Two signals disagreeing on one row - a check above "Turn failed" - is the
    // defect this row was filed about, so the mark follows the turn.
    const drawn = draw({ ...FULL, failed: true });

    expect(drawn, 'the row leads with the failure mark').toContain('i-x');
    expect(drawn, 'and not the check').not.toContain('i-check');
    // The tone rides on the class, so the mark alone does not say the row
    // failed: without this the mark could draw in the settled green.
    expect(drawn, 'and the mark carries the tone it is drawn in').toContain('class="ic st err"');
  });
});
