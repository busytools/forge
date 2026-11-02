import { readFileSync } from 'node:fs';

import { describe, expect, it } from 'vitest';

import type { SessionUpdate } from '../protocol';
import { repoPath } from '../repo';
import type { SessionSlot } from './types';
import {
  BOARD_REFUSAL_PREFIX,
  coversHome,
  fleetNews,
  HOME_IGNORES,
  HOME_WAKES,
  isBoardRefusal,
  REDRAWS,
  serviceReport,
} from './fleet';

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
  const source = readFileSync(repoPath('crates/forge-server/src/live.rs'), 'utf8');
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
  const source = readFileSync(repoPath('crates/forge-workspace/src/protocol.rs'), 'utf8');
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
  const source = readFileSync(repoPath('crates/forge-workspace/src/protocol.rs'), 'utf8');

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
  // A call's own output, which the row that asked for it draws: the fleet
  // region has no row for it.
  'call_output',
  // The handoff row is the seat's conversation, which its page draws; no
  // home row names the swap.
  'handoff',
  // The composer's own stand-down, which the session page draws in its box.
  'machine_turn',
  'notice',
  'set_mode_failed',
  'set_model_failed',
  'mcp_operation_error',
  'turn_complete',
  'hook_observation',
  'forge_account_identity',
  'dictate_overrides',
  'context_usage_snapshot',
  'mcp_snapshot',
  // The seat's own working tree, its monitors, the CLI's background registry,
  // its process walk and its two catalogues, which a page draws in its
  // inspector and its composer - and no home row does: a row's branch, count
  // and spinners come from the fleet's own read.
  //
  // The registry is the one of these a ROW reads, through
  // `has_background_work`, and its frame asks for that read through the
  // conversation the same event also arrives in: the CLI's
  // `background_tasks_changed` is a `chat_appended`, which both sides already
  // answer with a redraw (the server's own arm for it, and `chatNews` below),
  // and the clear on a dead connection rides with `connection_failed`. This
  // mirror is the inspector's copy of the same instant, so naming it here
  // would be a second read of one change.
  'background_tasks_changed',
  'monitors_changed',
  'work_changed',
  'processes_changed',
  'slash_commands_changed',
  'subagents_changed',
  'subagent_cards_changed',
  'file_index_changed',
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
  // The pile's own: a seat's queued prompts, which the session page draws
  // above its composer and no home row does.
  'prompt_queued',
  'prompt_cancel_resolved',
  'review_activity_notice',
  'dictate_started',
  'dictate_level',
  'dictate_transcribing',
  'dictate_progress',
  'dictate_ended',
  'dictate_models_changed',
  'fatal_error',
  // The seat's voice frames: its own page draws the control and plays the
  // audio, and no home row names either.
  'voice_audio',
  'voice_mode_state',
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
    // A result that FAILED is not a completion, but it is news: the rows
    // are re-read so the failure mark can arm - the server sends the same
    // frame as its own Redraw, and a client that dropped it left the mark
    // to the next unrelated re-read.
    expect(fleetNews(appended({ type: 'result', is_error: true, subtype: 'error' }))).toEqual({
      kind: 'redraw',
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
      { turn_error: { key: LEAD } },
      { turn_cancelled: { key: LEAD } },
      { permission_request: { key: LEAD } },
      { question_request: { key: LEAD } },
      { pending_interaction_resolved: { key: LEAD } },
      { slack_post_pending: { key: LEAD } },
      { slack_draft_resolved: { key: LEAD } },
      { browser_hand_off_pending: { key: LEAD } },
      { browser_hand_off_resolved: { key: LEAD } },
      { worker_status_changed: {} },
      // The seat going to sleep (#1930): the row moves to the closing mark,
      // and for a close made elsewhere this frame is the only one that says
      // so before the removal lands.
      { releasing: { key: LEAD } },
      // The turn's start (#1887). `queued` is the same news as `started`: the
      // roster goes Running the moment the prompt is routed, so the read the
      // first of the two asks for is answered with the turn already on.
      { prompt_lifecycle: { key: LEAD, uuid: 'p1', state: 'started' } },
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
   * that could have carried it - but only the ones whose data the snapshot
   * carries are worth a read. Gating a reader on `fleetNews` alone throws
   * the service status and the fatal error away, and the page keeps what it
   * read at subscribe for the life of the connection. The account pool is
   * the exception on the other side: it is a fleet redraw, so the region's
   * own table already carries it.
   */
  it('counts a slot-less update as one the home is sent when the snapshot carries it', () => {
    expect(coversHome({ service_status: { state: 'ok' } })).toBe(true);
    expect(coversHome({ fatal_error: { message: 'it died' } })).toBe(true);
    expect(coversHome('catalog_loaded')).toBe(true);
    expect(coversHome('accounts_changed')).toBe(true);
    // **A plugin update is not home news.** The inventory is the extensions
    // subject's read, and no `HomeWire` field carries it; the keyless arm
    // used to accept it as a refresh for an answer that could not differ.
    expect(coversHome({ plugins_inventory_updated: {} })).toBe(false);
    expect(coversHome({ plugins_update_run_progress: {} })).toBe(false);
    // A turn's own words on a watched seat are neither fleet news nor
    // slot-less, so a home subscriber is never sent them.
    expect(coversHome(appended({ type: 'assistant' }))).toBe(false);
    // **The board's own two do not reach the home** (#2517): their data left
    // the home's snapshot, so a home re-read could not answer for them.
    expect(coversHome({ cron_schedules_changed: { key: LEAD, crons: [] } })).toBe(false);
    expect(
      coversHome({
        connector_subscriptions_changed: { key: LEAD, gotify: [], slack: [] },
      }),
    ).toBe(false);
    // A task write still does: the fleet's counts are derived from the same
    // tasks.
    expect(coversHome({ tasks_changed: { key: LEAD, tasks: [] } })).toBe(true);
  });

  /**
   * The service line's own words, read off the update that carried them: it
   * is the page's only copy - the snapshot holds no such field - and a report
   * whose words were dropped would leave a refused edit reading as one that
   * landed.
   */
  it("reads the service line's own words", () => {
    expect(
      serviceReport({ service_status: { severity: 'error', message: 'the API is down' } }),
    ).toEqual({ severity: 'error', message: 'the API is down' });
    // A severity this client is older than reads as the warning it can word,
    // and a report with no words at all is no report.
    expect(
      serviceReport({ service_status: { severity: 'catastrophe', message: 'oy' } })?.severity,
    ).toBe('warning');
    expect(serviceReport({ service_status: { severity: 'warning', message: '' } })).toBeNull();
    expect(serviceReport({ service_status: { severity: 'warning' } })).toBeNull();
    expect(serviceReport({ tasks_changed: { key: LEAD, tasks: [] } })).toBeNull();
    expect(serviceReport('accounts_changed')).toBeNull();
  });

  /**
   * **The board's own reports, told from everyone else's.** The board page
   * draws the core's service line only when a board edit earned it, and what
   * tells the two apart is the words `board_edit_refused` writes - so the
   * format is read out of the Rust rather than trusted: a reworded refusal
   * would otherwise be a page that quietly stopped saying why.
   */
  it('knows a board refusal by the words the core writes it with', () => {
    const source = readFileSync(repoPath('crates/forge-workspace/src/workspace.rs'), 'utf8');
    const start = source.indexOf('fn board_edit_refused');
    if (start < 0) {
      throw new Error(
        'the workspace no longer holds `board_edit_refused`, so this mirror is unanchored',
      );
    }
    const body = source.slice(start, source.indexOf('\n    }\n', start));
    const literal = /message: format!\("([^"]*)"/.exec(body)?.[1];
    expect(literal, 'the refusal message is no longer a format literal this reads').toBeDefined();
    expect(
      literal?.startsWith(BOARD_REFUSAL_PREFIX),
      `the core's refusal no longer opens with "${BOARD_REFUSAL_PREFIX}" - the board would draw none`,
    ).toBe(true);

    // And the filter itself: any other producer's report is not the board's.
    expect(isBoardRefusal({ severity: 'warning', message: 'The board refused a move: nope' })).toBe(
      true,
    );
    expect(isBoardRefusal({ severity: 'error', message: 'could not spawn a worker for x' })).toBe(
      false,
    );
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
    expect(fleetNews({ mcp_operation_error: { key: LEAD } })).toEqual({ kind: 'nothing' });
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
      'this count and the enum disagree, and `SessionUpdate` held 72 variants when it was last ' +
        'set. Raise or lower it in the same edit that adds or removes one - the census below names ' +
        'the bucket an added variant belongs in - and if you moved no variant, the parse read a ' +
        'different set of names than the enum holds',
    ).toBe(72);
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

  /**
   * The mirror's own census: `REDRAWS` is read by no other check, so a junk
   * member or one that stranded a name in both buckets would survive tsc and
   * every suite, and the home would answer a name the core never sends.
   */
  it('keeps the redraw mirror to declared names, in one bucket only', () => {
    const { names } = serverVariantNames();
    const declared = new Set(names);

    const junk = [...REDRAWS].filter((name) => !declared.has(name));
    expect(junk, 'a redraw member the enum does not declare').toEqual([]);

    const twice = [...REDRAWS].filter((name) => NOT_NEWS.includes(name));
    expect(twice, "a name in both the client's redraw mirror and its not-news list").toEqual([]);
  });
});

/**
 * The home's coverage of every variant, which is the predicate a read is
 * spent on.
 *
 * `coversHome` decides whether one update is answered with a full snapshot
 * encode, and before this census its keyless half was "everything with no
 * seat": a plugin run's progress asked the home for an answer that cannot
 * carry it. The two tables in `fleet.ts` are the decision. This block holds
 * them to the enum and to the predicate, so a variant added, renamed or
 * dropped is a red build that names it rather than a page that quietly stops
 * following something or quietly reads for nothing.
 */
describe('the home variant census', () => {
  /**
   * The samples that cross as a bare string: the three unit variants, and
   * `accounts_changed` as a server older than its payload sends it - the
   * wake-up a client must still read for.
   */
  const UNIT = new Set([
    'catalog_loaded',
    'cli_version_changed',
    'accounts_changed',
    'dictate_availability',
  ]);

  /** One update of `name`, shaped so its own classification can read it. */
  function sample(name: string, keyed: Set<string>): SessionUpdate {
    if (name === 'chat_appended') {
      // The one variant whose answer is a field's, not a name's.
      return appended({ type: 'result', is_error: false, subtype: 'success' });
    }
    if (UNIT.has(name)) return name;
    return { [name]: keyed.has(name) ? { key: LEAD } : {} };
  }

  it('decides every declared variant, and the predicate follows the decision', () => {
    const { names, closed } = serverVariantNames();
    const news = new Set(serverArms().map((name) => snake(name)));
    const keyed = new Set(serverSlots().keyed.map((name) => snake(name)));

    // The parse's own denominator: a run that stopped early would find every
    // name it did read classified, which is the shape of a clean run over
    // nothing. The enum's variant count is asserted by the fleet census above.
    expect(closed, 'the parse never reached the end of `SessionUpdate`').toBe(true);

    // A variant is decided by one of four tables: a fleet arm, the fleet's
    // not-news list, or one of the home's two. A new variant in none is the
    // failure this census exists for.
    const unclassified = names.filter(
      (name) =>
        !news.has(name) &&
        !NOT_NEWS.includes(name) &&
        !HOME_WAKES.has(name) &&
        !HOME_IGNORES.has(name),
    );
    expect(
      unclassified,
      'a variant in no fleet arm, NOT_NEWS, HOME_WAKES or HOME_IGNORES is one nobody decided ' +
        'about: say whether the home reads for it, and why',
    ).toEqual([]);

    // The home's two tables cannot both claim a name, and an ignore cannot
    // shadow a fleet redraw except for the board pair that left the snapshot
    // (#2517). Anything else is a second, contradictory decision.
    const bothHome = names.filter((name) => HOME_WAKES.has(name) && HOME_IGNORES.has(name));
    expect(bothHome, 'a name in both home tables is a decision nobody can read').toEqual([]);
    const shadows = [...HOME_IGNORES.keys()].filter((name) => news.has(name));
    expect(
      shadows,
      'an ignored variant the fleet redraws for is a second decision over the same instant',
    ).toEqual(['cron_schedules_changed', 'connector_subscriptions_changed']);

    // And the predicate follows the decision, for every variant rather than
    // for the handful sampled above: an ignore wins over a fleet arm, a wake
    // reads, any other fleet arm reads, and the rest is silent.
    for (const name of names) {
      const expected = !HOME_IGNORES.has(name) && (HOME_WAKES.has(name) || news.has(name));
      expect(
        coversHome(sample(name, keyed)),
        `${name} ${expected ? 'changes' : 'cannot change'} the home's answer`,
      ).toBe(expected);
    }
  });

  /**
   * The decision itself, restated so it cannot move by accident.
   *
   * The census above derives its expectation from these two tables, so it
   * catches a variant nobody classified and a predicate that disagrees with
   * the tables - but not a table entry quietly dropped. This is the list a
   * person decided; a change to it is a change to what the home follows, and
   * that is worth reading in a diff.
   */
  it('holds the home tables to the set a person decided', () => {
    expect([...HOME_WAKES.keys()].sort()).toEqual([
      'accounts_changed',
      'catalog_loaded',
      'cli_version_changed',
      'dictate_availability',
      'dictate_models_changed',
      'fatal_error',
      'service_status',
      'worker_status_changed',
    ]);
    expect([...HOME_IGNORES.keys()].sort()).toEqual([
      'connector_subscriptions_changed',
      'cron_schedules_changed',
      'plugins_cli_action_failed',
      'plugins_cli_action_succeeded',
      'plugins_inventory_refresh_failed',
      'plugins_inventory_updated',
      'plugins_rollback_failed',
      'plugins_rollback_succeeded',
      'plugins_update_run_finished',
      'plugins_update_run_progress',
    ]);
  });

  it('carries no name the enum no longer declares, and states a reason for each', () => {
    const { names } = serverVariantNames();
    const declared = new Set(names);

    for (const [table, label] of [
      [HOME_WAKES, 'HOME_WAKES'],
      [HOME_IGNORES, 'HOME_IGNORES'],
    ] as const) {
      const stale = [...table.keys()].filter((name) => !declared.has(name));
      expect(stale, `${label} names a variant the enum does not declare`).toEqual([]);
      const unreasoned = [...table.entries()]
        .filter(([, why]) => why.trim() === '')
        .map(([name]) => name);
      expect(unreasoned, `${label} carries a row with no reason`).toEqual([]);
    }
  });

  /**
   * **Every keyless variant has a home decision, both ways round.**
   *
   * The server delivers every slot-less variant to a home subscriber
   * (`envelope.rs`), so a keyless variant nobody has decided about is one
   * that reaches the home and is silently dropped here - the failure this
   * block exists to make loud, and the forward check the fleet census makes
   * cannot see it. The board's own two are keyed on the project's lead and
   * are named for the snapshot's history rather than for the keyless arm.
   */
  it('decides every keyless variant, in exactly one home table', () => {
    const { keyed, seatless: seatlessNames } = serverSlots();
    const seatless = new Set(seatlessNames.map((name) => snake(name)));
    const keyedByDesign = new Set(['cron_schedules_changed', 'connector_subscriptions_changed']);

    expect(seatless.size, 'the seat-less arm was not read out of protocol.rs').toBeGreaterThan(3);
    // The two arms' own denominator. A conditional `slot()` arm belongs to
    // neither list, and every variant it names would then reach a subscription
    // this test never asks about.
    expect(
      keyed.length + seatlessNames.length,
      'a variant is in neither `slot()` arm, so neither this census nor the reader can place it',
    ).toBe(serverVariantNames().names.length);

    const wrong = [...HOME_WAKES.keys(), ...HOME_IGNORES.keys()].filter(
      (name) => !seatless.has(name) && !keyedByDesign.has(name),
    );
    expect(wrong, 'a table says slot-less about a variant that carries a seat').toEqual([]);

    const undecided = [...seatless].filter(
      (name) => !HOME_WAKES.has(name) && !HOME_IGNORES.has(name),
    );
    expect(
      undecided,
      'a keyless variant reaches every home subscriber and neither table decides its read: ' +
        'say which field of the snapshot carries it, or why none does',
    ).toEqual([]);
  });
});
