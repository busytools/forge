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
 * that recomputed it would disagree with the terminal the first time a turn
 * settled while nobody was watching, and it would disagree silently. This
 * answers one question only - is the fleet display now behind, and if so
 * about which seat.
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
export type FleetNews =
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
  // The account pool, which the band's own card draws and no row does. It is
  // the one of these nothing else in the stream mentions, so a quiet forge
  // emits no other update and the card would keep whatever it read at
  // subscribe.
  'accounts_changed',
  'dictate_availability',
  'connection_failed',
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
  // moved. The home's own row draws all three sections, so the update is a
  // redraw of that row and nothing the fleet reads.
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
 * Whether a `home` subscription hears this update - the server's own
 * `Subject::covers` for `Home`, and the one place it lives on this side.
 *
 * **`fleetNews` answers a different question and is not a substitute.**
 * That one says what the fleet region DRAWS; this one says what a home
 * subscriber is SENT, and the server sends more than the region draws. A
 * slot-less update belongs to no seat, so home is the only subscription that
 * could have carried it, and the slot-less arm exists so a client is not
 * left drawing what it read once at subscribe - which is what the service
 * status, the fatal error and the plugin records are.
 *
 * Gating a reader on `fleetNews` alone throws those away, and the symptom is
 * not an error: it is a page that read them once and never again.
 */
export function coversHome(update: SessionUpdate): boolean {
  return fleetNews(update).kind !== 'nothing' || slotOf(update) === null;
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
