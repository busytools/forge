import { readFileSync } from 'node:fs';

import { describe, expect, it } from 'vitest';

import type { SessionUpdate } from '../protocol';
import type { SessionSlot } from './types';
import { coversHome, fleetNews } from './fleet';

/**
 * The variant names the server's own `fleet_news` puts in one of its arms,
 * read out of `live.rs`.
 *
 * This file is a mirror of that function, and the failure it risks is the
 * server putting a variant in an arm this table does not know: the variant
 * draws, this calls it `nothing`, and the home stops following it with
 * nothing to report that it has. **No fixture can catch that** - the
 * classification is Rust code and never crosses the wire - so the arms are
 * read from the source instead.
 *
 * **The read assumes a shape, and changing it means re-checking this rather
 * than trusting a green:** the function is still `fleet_news` in `live.rs`;
 * its arms are still named `FleetNews::Redraw` and `FleetNews::Occupant`;
 * each is still written `A | B => Answer`, so the variant names sit in the
 * clause before the arrow and the answer begins the clause after it; and the
 * names are still spelled `SessionUpdate::Name`. An arm rewritten as
 * `=> { FleetNews::Redraw }`, or one reaching a variant through a wildcard,
 * reads as nothing, which the emptiness control below catches. A reshuffle
 * that still matches reads as a wrong answer, which nothing catches.
 *
 * Neither this nor the control can see a variant the server MOVES between
 * arms, or one it adds inside the `chat_appended` arm, where the decision is
 * a frame's own fields rather than a variant's name.
 */
function serverArms(arm: 'Redraw' | 'Occupant'): string[] {
  const source = readFileSync(
    new URL('../../../crates/forge-server/src/live.rs', import.meta.url),
    'utf8',
  );
  const start = source.indexOf('pub fn fleet_news');
  if (start < 0) {
    throw new Error('live.rs no longer holds `fleet_news`, so this mirror is unanchored');
  }
  const body = source.slice(start, source.indexOf('\n}\n', start));

  const names = new Set<string>();
  // An arm reads `A | B => Answer`, so the names are in the clause before the
  // arrow and the answer at the start of the clause after it.
  const clauses = body.split('=>');
  for (let at = 0; at < clauses.length - 1; at += 1) {
    if (!(clauses[at + 1] ?? '').trimStart().startsWith(`FleetNews::${arm}`)) continue;
    for (const match of (clauses[at] ?? '').matchAll(/SessionUpdate::([A-Za-z0-9]+)/g)) {
      if (match[1] !== undefined) names.add(match[1]);
    }
  }
  return [...names];
}

/** A variant name as the wire spells it. */
function snake(name: string): string {
  return name.replace(/([a-z0-9])([A-Z])/g, '$1_$2').toLowerCase();
}

/**
 * The variants the core's own `SessionUpdate::slot` answers `Some(key)` or
 * `None` for, read out of `protocol.rs`.
 *
 * `slotOf` is the other half of this file's claim about the server, and three
 * samples cannot see a variant that carries a `key: SessionSlot` and is not
 * recognised as one - so the list is read out of the method that decides it.
 */
function serverSlots(): { keyed: string[]; seatless: string[]; declared: number } {
  const source = readFileSync(
    new URL('../../../crates/forge-workspace/src/protocol.rs', import.meta.url),
    'utf8',
  );
  const start = source.indexOf('pub fn slot(&self)');
  if (start < 0) {
    throw new Error('protocol.rs no longer holds `slot`, so this mirror is unanchored');
  }
  const body = source.slice(start, source.indexOf('\n}\n', start));

  const names = (arm: string): string[] => {
    const found = new Set<string>();
    const clauses = body.split('=>');
    for (let at = 0; at < clauses.length - 1; at += 1) {
      if (!(clauses[at + 1] ?? '').trimStart().startsWith(arm)) continue;
      for (const match of (clauses[at] ?? '').matchAll(/Self::([A-Za-z0-9]+)/g)) {
        if (match[1] !== undefined) found.add(match[1]);
      }
    }
    return [...found];
  };

  // `slotOf` reads a payload's `key` and nothing else, so a seat-less variant
  // that ever grew one would be read as a seat. Counting the enum's own `key`
  // declarations is what catches that, because feeding a variant a key by
  // hand proves nothing - `slotOf` never looks at the variant's name.
  const enumStart = source.indexOf('pub enum SessionUpdate {');
  const enumBody = source.slice(enumStart, source.indexOf('\n}\n', enumStart));
  const declared = (enumBody.match(/^\s+key: SessionSlot,$/gm) ?? []).length;

  return { keyed: names('Some(key)'), seatless: names('None'), declared };
}

const LEAD: SessionSlot = { org: 'TestOrg', project: 'proj', label: 'lead' };

/** A `chat_appended` carrying one CLI frame, which is how the bulk of the stream arrives. */
function appended(frame: Record<string, unknown>): SessionUpdate {
  return { chat_appended: { key: LEAD, msg: frame } };
}

describe('what one update asks of the fleet', () => {
  /**
   * A turn's own words are most of the stream and no row shows one, so they
   * are `nothing` - and `nothing` is what keeps them out of the home store.
   * An assistant frame is the commonest of them.
   */
  it("draws nothing of a turn's own words", () => {
    expect(fleetNews(appended({ type: 'assistant' }))).toEqual({ kind: 'nothing' });
    expect(fleetNews(appended({ type: 'user' }))).toEqual({ kind: 'nothing' });
    // A result that FAILED is not a completion, and no row changes for it.
    expect(fleetNews(appended({ type: 'result', is_error: true, subtype: 'error' }))).toEqual({
      kind: 'nothing',
    });
  });

  /** A turn that finished well is the completion a row's mark is armed from. */
  it('calls a successful result a completion on its seat', () => {
    expect(fleetNews(appended({ type: 'result', is_error: false, subtype: 'success' }))).toEqual({
      kind: 'completed',
      slot: LEAD,
    });
  });

  /**
   * Work starting again is its own arm, because it clears a mark where a
   * completion arms one - a session-state frame that says anything else is
   * a redraw with no seat attached.
   */
  it('tells a seat starting work from a frame that merely redraws', () => {
    expect(
      fleetNews(appended({ type: 'system', subtype: 'session_state_changed', state: 'running' })),
    ).toEqual({ kind: 'running', slot: LEAD });
    expect(
      fleetNews(appended({ type: 'system', subtype: 'session_state_changed', state: 'idle' })),
    ).toEqual({ kind: 'redraw' });
    expect(fleetNews(appended({ type: 'system', subtype: 'background_tasks_changed' }))).toEqual({
      kind: 'redraw',
    });
  });

  /** A fresh occupant takes the seat, which is the row set changing. */
  it('calls a new occupant an occupant, on its seat', () => {
    expect(fleetNews({ spawning: { key: LEAD, display_name: 'forge' } })).toEqual({
      kind: 'occupant',
      slot: LEAD,
    });
    expect(fleetNews({ connected: { key: LEAD } })).toEqual({ kind: 'occupant', slot: LEAD });
    expect(fleetNews({ session_replaced: { key: LEAD } })).toEqual({
      kind: 'occupant',
      slot: LEAD,
    });
  });

  /**
   * The redraw set is a list rather than a prefix rule, and these are the
   * variants on it. A member dropped from it is a row that stops updating
   * when that thing happens, which nothing else would report.
   */
  it('redraws for the variants that change what a row or a card says', () => {
    const redraws = [
      'catalog_loaded',
      'cli_version_changed',
      'dictate_availability',
      { connection_failed: { key: LEAD } },
      { auth_required: { key: LEAD } },
      { turn_error: { key: LEAD } },
      { turn_cancelled: { key: LEAD } },
      { permission_request: { key: LEAD } },
      { question_request: { key: LEAD } },
      { pending_interaction_resolved: { key: LEAD } },
      { worker_status_changed: {} },
      { peer_inflight_stats_changed: { key: LEAD } },
    ];
    for (const update of redraws) {
      expect(fleetNews(update as SessionUpdate), JSON.stringify(update)).toEqual({
        kind: 'redraw',
      });
    }
  });

  /**
   * What a home subscriber is SENT, which is not what the region DRAWS. A
   * slot-less update belongs to no seat, so home is the only subscription
   * that could have carried it, and the service status, the fatal error and
   * the plugin records all arrive that way. Gating a reader on `fleetNews`
   * alone throws them away, and the page keeps what it read at subscribe for
   * the life of the connection.
   */
  it('counts a slot-less update as one the home is sent', () => {
    expect(coversHome({ service_status: { state: 'ok' } })).toBe(true);
    expect(coversHome({ fatal_error: { message: 'it died' } })).toBe(true);
    expect(coversHome({ plugins_inventory_updated: {} })).toBe(true);
    expect(coversHome('catalog_loaded')).toBe(true);
    // A turn's own words on a watched seat are neither fleet news nor
    // slot-less, so a home subscriber is never sent them.
    expect(coversHome(appended({ type: 'assistant' }))).toBe(false);
  });

  /**
   * `slotOf` is the other half of this file's claim about the server, and what
   * it can go wrong on is narrow: it reads a payload's `key` and never the
   * variant's name, so the one thing that would break it is a variant the
   * core gives no seat and that carries a `key` anyway.
   *
   * The samples that show it working are in `socket.test.ts`. This is the arm
   * that can fail: every `key: SessionSlot` the enum declares has to belong to
   * a variant `slot()` answers for.
   */
  it('finds no seat the core does not give, and every one it does', () => {
    const { keyed, seatless, declared } = serverSlots();
    expect(keyed.length, 'the slot arm was not read out of protocol.rs at all').toBeGreaterThan(20);
    expect(
      seatless.length,
      'the seat-less arm was not read out of protocol.rs at all',
    ).toBeGreaterThan(3);

    expect(declared, 'a seat-less variant carries a key, and slotOf would read it as a seat').toBe(
      keyed.length,
    );
  });

  /** Everything the fleet does not draw, which is most of the stream. */
  it('draws nothing of the variants no row shows', () => {
    expect(fleetNews({ hook_observation: { key: LEAD } })).toEqual({ kind: 'nothing' });
    expect(fleetNews({ status_snapshot: { key: LEAD } })).toEqual({ kind: 'nothing' });
    expect(fleetNews({ dictate_level: { key: LEAD } })).toEqual({ kind: 'nothing' });
  });

  /**
   * The mirror, checked against the server's own arms rather than against
   * this file's idea of them. A variant the server draws for and this table
   * does not know is a row that stops updating, silently.
   */
  it('knows every variant the server draws a redraw or an occupant for', () => {
    const redraws = serverArms('Redraw');
    const occupants = serverArms('Occupant');

    // A control: an extractor that read nothing would make the loop below
    // pass for ever, which is a green that means the test is broken.
    expect(redraws.length, 'the redraw arm was not read out of live.rs at all').toBeGreaterThan(5);
    expect(occupants.length, 'the occupant arm was not read out of live.rs at all').toBe(3);

    for (const name of redraws) {
      expect(fleetNews({ [snake(name)]: { key: LEAD } }), `${name} redraws on the server`).toEqual({
        kind: 'redraw',
      });
    }
    for (const name of occupants) {
      expect(
        fleetNews({ [snake(name)]: { key: LEAD } }),
        `${name} is an occupant on the server`,
      ).toEqual({ kind: 'occupant', slot: LEAD });
    }
  });
});
