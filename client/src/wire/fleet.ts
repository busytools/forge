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
const REDRAWS = new Set([
  'catalog_loaded',
  'cli_version_changed',
  'dictate_availability',
  'connection_failed',
  'auth_required',
  'turn_error',
  'turn_cancelled',
  'permission_request',
  'question_request',
  'pending_interaction_resolved',
  'worker_status_changed',
  'peer_inflight_stats_changed',
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
    return isSuccessResult(frame) ? { kind: 'completed', slot } : NOTHING;
  }
  if (frame['type'] !== 'system') return NOTHING;
  if (frame['subtype'] === 'background_tasks_changed') return { kind: 'redraw' };
  if (frame['subtype'] !== 'session_state_changed') return NOTHING;
  // `state` sits on the frame itself: the generic system repr flattens the
  // frame's own fields into `data`.
  return frame['state'] === 'running' ? { kind: 'running', slot } : { kind: 'redraw' };
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
