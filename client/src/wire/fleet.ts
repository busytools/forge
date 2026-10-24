/**
 * What one update asks of the home's fleet region.
 *
 * A mirror of `crates/forge-server/src/live.rs`'s `fleet_news`, and it is a
 * mirror rather than a re-derivation for a reason: the server keeps one
 * table for two readers - its own view fold, and the socket deciding which
 * subscribers an update belongs to - and a client that wants either answer
 * has to keep the same table or disagree with it.
 *
 * **Nothing here decides what a row SAYS.** A row's state is the server's,
 * folded from the core's own `Live` and carried in the snapshot: a client
 * that recomputed it would disagree with the server's own fold the first
 * time a turn settled while nobody was watching, and it would disagree
 * silently. This answers one question only - is the fleet display now behind,
 * and if so about which seat.
 *
 * **A second copy of a table the server keeps singular, and that is the
 * cost.** `envelope.rs` says a second copy "would both drift and hand a home
 * subscriber every token of every seat in the fleet"; this one exists
 * because the wire does not say which subscription an update was sent for,
 * and it takes the drift rather than the tokens. The drift is the day the
 * server puts a variant in its redraw or its occupant arm and this table
 * does not know it: that variant draws, this calls it `nothing`, and the
 * home stops following it with nothing to report that it has.
 *
 * `fleet.test.ts` reads the server's arms out of `live.rs` and fails when
 * either set here is narrower than the set there, so that day is a red build
 * rather than a silent one. That read cannot see a variant the server DROPS
 * from the redraw arm back to the wildcard - a move between the two arms is
 * caught, because the occupant count it asserts is exact - nor one the server
 * adds inside the `chat_appended` arm, where the decision is a frame's own
 * fields rather than a variant's name. The census beside it closes the drop,
 * by starting from the enum rather than from the arms.
 */

import type { SessionUpdate } from '../protocol';
import { slotOf } from '../protocol';
import type { SessionSlot } from './types';

/** What one update asks of the fleet region. */
type FleetNews =
  | { kind: 'nothing' }
  | { kind: 'redraw' }
  | { kind: 'completed'; slot: SessionSlot }
  | { kind: 'running'; slot: SessionSlot }
  | { kind: 'occupant'; slot: SessionSlot };

const NOTHING: FleetNews = { kind: 'nothing' };

/**
 * The variants that redraw a row or a card, none of which carries a payload
 * the fleet reads. The catalog, the dictation snapshot and the claude
 * version all arrive after a listener binds, so a page opened in those first
 * seconds would otherwise keep the empty answer it painted.
 */
export const REDRAWS = new Set([
  'catalog_loaded',
  'cli_version_changed',
  // The account pool, which the band's own card draws and no row does. Its
  // frame carries the pool's own state, so a usage-only move is applied
  // without a re-read (`home/live.ts`); a move in what a project row binds on
  // still reads. It stays in this set because it is news the home draws.
  'accounts_changed',
  'dictate_availability',
  'connection_failed',
  // A document hand-off draws no row, and its ask rides this arm because
  // the home subscription is the one EVERY client holds: the raise has to
  // reach the reader wherever they are (measured 2026-10-12 - seat-keyed,
  // it reached nobody sitting on the home). The resolution rides it too, so
  // a view that never answered the ask closes it.
  'document_hand_off_pending',
  'document_hand_off_resolved',
  // The seat's release has begun: its row moves to the going-to-sleep mark
  // until the roster lands it asleep or gone. A close made from ANOTHER view
  // sends nothing else before the removal, so without this arm that viewer's
  // rail keeps reading the row as working through the teardown (#1930).
  'releasing',
  'auth_required',
  'turn_error',
  'turn_cancelled',
  'permission_request',
  'question_request',
  'pending_interaction_resolved',
  // The turn's start, and the earliest frame that says a turn was accepted:
  // the roster's `running` comes from `turn_pending`, stamped when the prompt
  // is routed, so the read this asks for is answered with the turn already
  // on. Every state earns it - `queued` and `started` are the start, and
  // `state` is free-form by design, so a state this build has not seen has to
  // move a page rather than be dropped; `completed` sits at the boundary the
  // result frame also announces, and the terminal states are a prompt that
  // will not run, where the read can only show what the core holds. The reads
  // are coalesced, so the boundary ones fold into the read the result asks
  // for. Without this arm a row kept the idle it last read until an unrelated
  // frame happened to be news (#1887, measured at ~40s).
  'prompt_lifecycle',
  // The third kind of ask (#1758): a held Slack draft moves its seat's row
  // exactly as the two above do, and the core now says so in the lifecycle -
  // the row can only draw it if this side re-reads on the news.
  'slack_post_pending',
  'slack_draft_resolved',
  'browser_hand_off_pending',
  'browser_hand_off_resolved',
  'worker_status_changed',
  // The project's task set, its schedules and its connector subscriptions
  // moved. Since #2517 the schedules and connector sets ride the board's
  // subject; the home still hears the task write, because its fleet counts
  // are derived from the same tasks, so this stays a redraw here.
  'tasks_changed',
  'cron_schedules_changed',
  'connector_subscriptions_changed',
]);

/** Whether a `Result` frame is a turn that finished well. */
function isSuccessResult(msg: Record<string, unknown>): boolean {
  return msg['is_error'] === false && msg['subtype'] === 'success';
}

/**
 * A `chat_appended` frame, which is the bulk of the stream and the one arm
 * that has to look inside the CLI's own message rather than at a variant
 * name: a turn's own words are most of it, and no row shows one.
 */
function chatNews(payload: Record<string, unknown>): FleetNews {
  const msg = payload['msg'];
  if (msg === null || typeof msg !== 'object') return NOTHING;
  const frame = msg as Record<string, unknown>;
  const slot = slotOf({ chat_appended: payload });
  if (slot === null) return NOTHING;

  if (frame['type'] === 'result') {
    // A failure is a re-read, not a silence: it arms the rail's failure
    // mark, so the rows have to come back. The server classifies the same
    // frame as its own Redraw, and a cancelled turn redraws the rows it
    // left the same way.
    return isSuccessResult(frame) ? { kind: 'completed', slot } : { kind: 'redraw' };
  }
  if (frame['type'] !== 'system') return NOTHING;
  if (frame['subtype'] === 'background_tasks_changed') return { kind: 'redraw' };
  if (frame['subtype'] !== 'session_state_changed') return NOTHING;
  // `state` sits on the frame itself: the generic system repr flattens the
  // frame's own fields into `data`.
  return frame['state'] === 'running' ? { kind: 'running', slot } : { kind: 'redraw' };
}

/**
 * The updates that belong to no seat and whose data a field of the home
 * snapshot carries, with that field.
 *
 * **Slot-less is not the same as home news.** A variant that names no seat
 * reaches only a home subscriber, so it has no other reader to lean on - but
 * the read it wakes has to be able to come back different. These are the
 * eight where it can: each has a field on `HomeWire` that moves with it.
 *
 * A variant belongs here by what the home's ANSWER holds, not by what its
 * name suggests: `DictateModelsChanged` is here because the band's dictation
 * card counts `HomeWire.dictate.snapshot.models`, while the catalogue the
 * update carries is the models page's own read.
 *
 * **`AccountsChanged` is the one of these that carries its state**, so the
 * home applies a usage-only move without a read (`home/live.ts`) - while a
 * move in the loading rows, the selection signal or a probe failure still
 * reads. It is listed here because the field it moves is the snapshot's, and
 * a frame from a server older than the payload is still the wake-up this
 * table names.
 *
 * `fleet.test.ts` enumerates every declared variant and holds this table and
 * `HOME_IGNORES` to the enum, so a new variant is a red build rather than a
 * page that quietly stops following something.
 */
export const HOME_WAKES: ReadonlyMap<string, string> = new Map([
  ['catalog_loaded', 'HomeWire.projects, the roster the catalog scan fills'],
  ['cli_version_changed', 'HomeWire.cli_version'],
  ['accounts_changed', 'HomeWire.accounts, folded from the frame when it carries one'],
  [
    'worker_status_changed',
    'HomeWire.fleet, whose live-worker count and unaccounted miss it moves',
  ],
  ['dictate_availability', 'HomeWire.dictate, whose models are resident once it lands'],
  ['dictate_models_changed', 'HomeWire.dictate.snapshot.models, whose rows the preflight resolves'],
  ['service_status', 'HomeWire.service_status, the issue the core holds'],
  ['fatal_error', 'HomeWire.fatal_error, the words the core stopped with'],
]);

/**
 * The slot-less updates a home subscriber can be sent whose data no field of
 * the snapshot carries.
 *
 * The `Plugins*` family is the set: the plugin inventory is the `extensions`
 * subject's whole read, and the home's snapshot has never carried it. They
 * are keyless, so before this table each one bought a full home read for an
 * answer that could not differ - the Extensions page's own subscription is
 * where they belong.
 *
 * The other two are keyed, not slot-less, and they are here for history:
 * `fleetNews` redraws for them because the board's row draws their sets, and
 * a cron or a connector set left the home's snapshot with the board (#2517).
 * Naming them in a table keyed by home coverage, rather than by the server's
 * slot-less arm, is what keeps one decision in one place.
 */
export const HOME_IGNORES: ReadonlyMap<string, string> = new Map([
  [
    'plugins_inventory_updated',
    'the inventory is the extensions subject, and no HomeWire field carries it',
  ],
  [
    'plugins_inventory_refresh_failed',
    'the failed refresh is the extensions subject, and no HomeWire field carries it',
  ],
  [
    'plugins_cli_action_succeeded',
    'an action outcome is the extensions subject, and no HomeWire field carries it',
  ],
  [
    'plugins_cli_action_failed',
    'an action refusal is the extensions subject, and no HomeWire field carries it',
  ],
  [
    'plugins_update_run_progress',
    'the update run is the extensions subject, and no HomeWire field carries it',
  ],
  [
    'plugins_update_run_finished',
    'the finished run is the extensions subject, and no HomeWire field carries it',
  ],
  [
    'plugins_rollback_succeeded',
    'the rollback result is the extensions subject, and no HomeWire field carries it',
  ],
  [
    'plugins_rollback_failed',
    'the failed rollback is the extensions subject, and no HomeWire field carries it',
  ],
  ['cron_schedules_changed', 'the schedules left the home for the board (#2517)'],
  ['connector_subscriptions_changed', 'the subscriptions left the home for the board (#2517)'],
]);

/**
 * Whether one update is worth a fresh home read.
 *
 * **Three questions look alike and only one of them is this one.** The
 * server's `Subject::covers` for `Home` says what a home subscriber is SENT;
 * `fleetNews` says what the fleet region DRAWS; and this says what the
 * client SPENDS a read on. A read is a full encode on the server, so it is
 * owed only when the answer can come back different - which is the union of
 * the fleet's own table and the keyless facts above, and is deliberately
 * narrower than the server's delivery: the plugin family reaches a home
 * subscriber because it names no seat, and no field of the snapshot carries
 * it.
 *
 * **Gating a reader on `fleetNews` alone throws the keyless facts away**, and
 * the symptom is not an error: it is a page that read them once and never
 * again. Gating it on "slot-less" instead is the other failure - a keyless
 * update whose data the snapshot cannot carry asks for a read that cannot
 * change anything. Both tables are the answer, and neither subsumes the
 * other.
 */
export function coversHome(update: SessionUpdate): boolean {
  const variant = typeof update === 'string' ? update : Object.keys(update)[0];
  // The ignore wins over the fleet's own arm: the board pair redraws a row
  // the home does not carry, and a plugin frame reaches a home subscriber
  // because it names no seat rather than because the home draws it.
  if (variant !== undefined && HOME_IGNORES.has(variant)) return false;
  if (fleetNews(update).kind !== 'nothing') return true;
  return variant !== undefined && HOME_WAKES.has(variant);
}

/** One service report's own words, narrowed where it enters. */
export interface ServiceReport {
  severity: 'warning' | 'error';
  message: string;
}

/**
 * One service report, narrowed where it enters - off an update's payload or
 * off the home snapshot's own `service_status`, which carry the same shape.
 *
 * A report with nothing wordable is no report: `error` is the only severity
 * that escalates, anything else is the warning every producer defaults to,
 * and a blank message would draw a line saying nothing.
 */
export function reportFrom(value: unknown): ServiceReport | null {
  const report = value as { severity?: unknown; message?: unknown } | null | undefined;
  const message = report?.message;
  if (typeof message !== 'string' || message === '') return null;
  return { severity: report?.severity === 'error' ? 'error' : 'warning', message };
}

/**
 * The core's service line, when this update is one.
 *
 * The report is keyless on the wire, so every home subscriber is sent it. The
 * board is one of its producers: an edit the core refused says so here, and a
 * page that folded the update into a re-read alone would leave a press that
 * did nothing and a press the core refused reading the same.
 */
export function serviceReport(update: SessionUpdate): ServiceReport | null {
  if (typeof update === 'string') return null;
  return reportFrom((update as { service_status?: unknown }).service_status);
}

/**
 * The words every board refusal opens with.
 *
 * It is the contract between the core's service line and the board page: the
 * refusal a board edit earns is written by `board_edit_refused` in the
 * workspace, and the board draws only reports that are its own - the service
 * line carries every producer's, and a failed worker spawn is not news about
 * the board. `fleet.test.ts` reads the format out of the Rust source, so
 * rewording it is a red build rather than a page that stops saying why.
 */
export const BOARD_REFUSAL_PREFIX = 'The board refused';

/** Whether a service report is one a board edit earned. */
export function isBoardRefusal(report: ServiceReport): boolean {
  return report.message.startsWith(BOARD_REFUSAL_PREFIX);
}

/** Classify one update for the fleet region. */
export function fleetNews(update: SessionUpdate): FleetNews {
  if (typeof update === 'string') {
    return REDRAWS.has(update) ? { kind: 'redraw' } : NOTHING;
  }
  const [variant] = Object.keys(update);
  if (variant === undefined) return NOTHING;

  const payload = update[variant];
  const fields =
    payload !== null && typeof payload === 'object' ? (payload as Record<string, unknown>) : {};

  if (variant === 'chat_appended') return chatNews(fields);

  // The row set, and what each row is.
  if (variant === 'spawning' || variant === 'connected' || variant === 'session_replaced') {
    const slot = slotOf(update);
    return slot === null ? NOTHING : { kind: 'occupant', slot };
  }

  return REDRAWS.has(variant) ? { kind: 'redraw' } : NOTHING;
}
