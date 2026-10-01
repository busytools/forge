import { render } from 'svelte/server';
import { describe, expect, it, vi } from 'vitest';

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

  it('counts the wait since the last frame into a running row, and nothing into a settled one', () => {
    // Both directions of the running branch are user-visible. Dropping the
    // settled one makes a settled row report wall-clock since its stamp -
    // time the turn never spent - and collapsing the derived to the record's
    // span freezes the running clock the ticker exists to move. The clock is
    // frozen so both sides read exactly.
    vi.useFakeTimers();
    vi.setSystemTime(new Date('2026-10-01T06:00:00.000Z'));
    try {
      const running: TurnInfo = {
        ...FULL,
        running: true,
        duration_ms: 40_000,
        ended_at_utc: '2026-10-01T05:58:30.000Z',
        api_ms: null,
        session_cost_usd: null,
      };
      const summary = (info: TurnInfo): string => {
        const body = draw(info);
        return body.slice(body.indexOf('<summary'), body.indexOf('</summary>'));
      };

      expect(summary(running), 'the span plus the 1m 30s wait since the last frame').toContain(
        '2m 10s',
      );
      expect(summary(FULL), 'and a settled row reads its own span, not wall-clock since').toContain(
        '2m 41s',
      );
    } finally {
      vi.useRealTimers();
    }
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

  it('draws the running row what the frames carry, and no slot for the cost it lacks', () => {
    // While the turn runs the row leads with a ring and shows the figures the
    // frames already carry. The cumulative cost is settle-only, so a running
    // row has none - and a slot with no value is not drawn at all: `- cumulative`
    // reads as "cumulative nothing" rather than as a figure still to arrive,
    // and a reader cannot tell that from a broken one. The record keeps FULL's
    // end stamp on purpose: the `ended` fact must dash on the RUNNING arm, not
    // because the stamp is absent.
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
    expect(summary, 'and no cost segment at all where the record carries none').not.toContain(
      'cumulative',
    );
    expect(summary, 'claiming no settled figure it has not been given').not.toContain('$');
    expect(facts(body), 'nor an end it does not have').toContain('ended=-');
  });

  it('draws the cost once the frames carry one', () => {
    // The control for the arm above: dropping the whole segment rather than
    // its empty state would pass that assertion and lose the figure.
    const running: TurnInfo = { ...FULL, running: true, api_ms: null };
    const body = draw(running);
    const summary = body.slice(body.indexOf('<summary'), body.indexOf('</summary>'));

    expect(summary, 'the cumulative cost, which only a settled Result carries').toContain(
      '$4.82 cumulative',
    );
  });

  it('carries the disclosure chevron the rest of the column draws, and no spelled-out verb', () => {
    // Every other `<details>` here leads its toggle with the shared chevron,
    // so this row spelling `expand` instead is a second convention for one
    // job. The summary keeps its own text either way, which is what a reader
    // who cannot see the glyph is told the disclosure is.
    const body = draw(FULL);

    // One chevron, and the one the stylesheet's open/closed rules reach: a
    // second glyph on the row would be a second answer to the same question.
    expect(body.match(/#i-chev/g), 'exactly one disclosure chevron').toHaveLength(1);
    expect(body, 'and it is the shared one the stylesheet turns').toContain('class="ic arw"');
    expect(body, 'and not the verb spelled out in its place').not.toMatch(
      />\s*(expand|collapse)\s*</,
    );
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
