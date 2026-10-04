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
 * Neither this nor the control can see a variant the server DROPS from the
 * redraw arm back to the wildcard. A move between the two arms is caught,
 * because the occupant count the test below asserts is exact; only the drop
 * is not. Nor can either see one the server adds inside the `chat_appended`
 * arm, where the decision is a frame's own fields rather than a variant's
 * name. The census at the foot of this file is what closes the drop.
 *
 * Called with no answer it returns every name in any arm, which is the
 * server's half of that census. A name spelled in a comment between two arms
 * is read as an arm's here, so the census inherits the assumption above: a
 * comment naming a variant the wildcard reaches would satisfy it wrongly.
 */
function serverArms(arm?: 'Redraw' | 'Occupant'): string[] {
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
    const answers = (clauses[at + 1] ?? '').trimStart();
    if (arm !== undefined && !answers.startsWith(`FleetNews::${arm}`)) continue;
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

/**
 * Every variant name `SessionUpdate` declares, as it crosses the wire, read
 * off the enum in `crates/forge-workspace/src/protocol.rs`.
 *
 * The derivation is serde's own for this enum: `rename_all = "snake_case"`,
 * so a variant's name snake-cased is the name it crosses under. `closed`
 * reports whether the parse reached the enum's closing brace, and it is the
 * denominator the census below needs - a parse that stopped early answers
 * with a short list, and every name it lost is a name the census then finds
 * classified.
 *
 * **The read assumes a shape:** the enum still opens with `pub enum
 * SessionUpdate {` at column zero and closes with `}` at column zero, and
 * every variant is still spelled at the top level of that body. The
 * derivation implements `rename_all` and not an explicit `#[serde(rename)]`,
 * which no variant of this enum carries. The name it reads is the variant's
 * own, so a rename is invisible here in either bucket: a variant no bucket
 * names reads as a name the census fails on, one in `NOT_NEWS` leaves a name
 * in that list which is no longer the wire name, and one an arm names keeps
 * the census green while the wire name has moved.
 */
function serverVariantNames(): { names: string[]; closed: boolean } {
  const source = readFileSync(
    new URL('../../../crates/forge-workspace/src/protocol.rs', import.meta.url),
    'utf8',
  );

  const names: string[] = [];
  let closed = false;
  let inside = false;
  for (const line of source.split('\n')) {
    if (!inside) {
      inside = line.startsWith('pub enum SessionUpdate {');
      continue;
    }
    if (line === '}') {
      closed = true;
      break;
    }
    const trimmed = line.trim();
    // A doc comment, an attribute or a blank line says nothing about the
    // variants. A field inside a variant's body falls out below, by not
    // starting with an uppercase letter.
    if (trimmed === '' || trimmed.startsWith('//') || trimmed.startsWith('#[')) continue;
    const variant = /^[A-Za-z0-9_]+/.exec(trimmed)?.[0] ?? '';
    if (!/^[A-Z]/.test(variant)) continue;
    names.push(snake(variant));
  }
  return { names, closed };
}

/**
 * The variants `fleet_news` answers `Nothing` for through its wildcard arm
 * rather than by naming them.
 *
 * A variant belongs here only when the fleet region really does draw nothing
 * of it: the conversation frames, the dictation frames, the account and
 * plugin snapshots. Most of the stream is here.
 *
 * It is a list rather than a rule because whether an update is news is a
 * decision about a row and not a property of a name, so a rule that guessed
 * would classify a new variant silently - which is the failure the census
 * below exists to make loud. Adding a variant to the enum puts it in neither
 * this list nor an arm, and the census goes red until it is in one of them.
 */
const NOT_NEWS: readonly string[] = [
  'history_replayed',
  'slash_command_error',
  'notice',
  'runtime_reload_completed',
  'runtime_reload_failed',
  'set_mode_failed',
  'set_model_failed',
  'mcp_operation_error',
  'turn_complete',
  'hook_observation',
  'status_snapshot',
  'forge_account_identity',
  'dictate_overrides',
  'dictate_device_pin',
  'oauth_credentials_snapshot',
  'context_usage_snapshot',
  'mcp_snapshot',
  // The seat's own working tree, its monitors, the CLI's background registry,
  // its process walk and its two catalogues, which a page draws in its
  // inspector and its composer - and no home row does: a row's branch, count
  // and spinners come from the fleet's own read.
  'background_tasks_changed',
  'monitors_changed',
  'work_changed',
  'processes_changed',
  'slash_commands_changed',
  'subagents_changed',
  'dispatches_changed',
  'file_index_changed',
  'sessions_listed',
  'service_status',
  'plugins_inventory_updated',
  'plugins_inventory_refresh_failed',
  'plugins_cli_action_succeeded',
  'plugins_cli_action_failed',
  'plugins_update_run_progress',
  'plugins_update_run_finished',
  'plugins_rollback_succeeded',
  'plugins_rollback_failed',
  'peer_envelope_appended',
  'gotify_notification_appended',
  'cron_prompt_appended',
  'slack_message_appended',
  'slack_post_pending',
  'slack_draft_resolved',
  'prompt_queued_while_busy',
  'review_activity_notice',
  'dictate_started',
  'dictate_level',
  'dictate_transcribing',
  'dictate_progress',
  'dictate_ended',
  'fatal_error',
];

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
      'accounts_changed',
      'dictate_availability',
      { connection_failed: { key: LEAD } },
      { auth_required: { key: LEAD } },
      { turn_error: { key: LEAD } },
      { turn_cancelled: { key: LEAD } },
      { permission_request: { key: LEAD } },
      { question_request: { key: LEAD } },
      { pending_interaction_resolved: { key: LEAD } },
      { worker_status_changed: {} },
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
   *
   * The account pool is the one of these that reads as a live state rather
   * than as an event: a card that keeps `0 ready, probing` while the pool has
   * been ready for minutes looks like a slow probe rather than like a page
   * that stopped listening.
   */
  it('counts a slot-less update as one the home is sent', () => {
    expect(coversHome({ service_status: { state: 'ok' } })).toBe(true);
    expect(coversHome({ fatal_error: { message: 'it died' } })).toBe(true);
    expect(coversHome({ plugins_inventory_updated: {} })).toBe(true);
    expect(coversHome('catalog_loaded')).toBe(true);
    expect(coversHome('accounts_changed')).toBe(true);
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

/**
 * The census: every variant the enum declares is in one of the two buckets,
 * and each is in exactly one.
 *
 * **What the arms above cannot see, and this can.** A variant the server
 * adds, or one it drops from an arm, is classified by `fleet_news`'s
 * wildcard as `Nothing` - and the home stops following that seat with the
 * suite green, which is a page that reads as quiet rather than as broken.
 * Reading the arms alone cannot report it, because the arms are the thing
 * that went silent. Starting from the enum is what turns it into a red test
 * naming the variant.
 *
 * The bucket a variant lands in is a decision, so the not-news list is
 * written out rather than derived: `Nothing` is the right answer for most of
 * the stream and the wrong one for a row, and nothing but a person can tell
 * which a new variant is.
 */
describe('the variant census', () => {
  it('classifies every variant the core can send', () => {
    const { names, closed } = serverVariantNames();
    const news = new Set(serverArms().map((name) => snake(name)));

    // The denominators, because an emptiness assertion below that has stopped
    // reading anything looks exactly like a clean one. `closed` is the parse
    // reaching the enum's own closing brace, and the count is the enum's own
    // number of variants: a parse that read a different count than the enum
    // holds cannot pass as a clean run.
    expect(
      closed,
      'the parse never reached the end of `SessionUpdate`, so it is the parse that moved and not ' +
        'the classification',
    ).toBe(true);
    expect(
      names.length,
      'this count and the enum disagree, and `SessionUpdate` held 65 variants when it was last ' +
        'set. Raise or lower it in the same edit that adds or removes one - the census below names ' +
        'the bucket an added variant belongs in - and if you moved no variant, the parse read a ' +
        'different set of names than the enum holds',
    ).toBe(65);
    expect(news.size, 'the `fleet_news` arms were not read out of live.rs at all').toBeGreaterThan(
      5,
    );

    const unclassified = names.filter((name) => !news.has(name) && !NOT_NEWS.includes(name));
    expect(
      unclassified,
      'a variant in neither a `fleet_news` arm nor the not-news list is one the home stops ' +
        'following in silence: name it in the arm it belongs in, or add it to NOT_NEWS when no ' +
        'row draws it',
    ).toEqual([]);
  });

  it('classifies each variant once', () => {
    const news = new Set(serverArms().map((name) => snake(name)));
    const twice = NOT_NEWS.filter((name) => news.has(name));

    expect(
      twice,
      'a variant in both an arm and the not-news list is a decision nobody can read',
    ).toEqual([]);
  });

  it('carries no name the enum no longer declares', () => {
    const { names } = serverVariantNames();
    const declared = new Set(names);
    const stale = NOT_NEWS.filter((name) => !declared.has(name));

    expect(
      stale,
      'the not-news list carries a name the enum read does not declare: either the enum dropped ' +
        'the variant or the read missed it',
    ).toEqual([]);
  });
});
