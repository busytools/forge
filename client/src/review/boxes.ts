/**
 * The shapes the review page's boxes share: the page holds their state, the
 * canvas and the two halves draw them, and `WordsBox` is the one component
 * behind every one of them.
 *
 * The state lives on the PAGE because a take's words and its own line have to
 * reach the box that began the take, and the canvas rebuilds a row whenever it
 * moves one - a draft a box held itself would go with the row.
 */

import type { Dictation, RowPhase, TakeLine } from '../composer/dictate.svelte';
import type { Bind, Mode } from '../composer/dictate-key';
import type { ComposerConnection } from '../composer/view';
import type { Take } from '../composer/wire';
import type { DictateAxes } from '../session/wire';
import type { ReviewSpot } from './write';

/**
 * One box's words, and where its take is.
 *
 * **The take's own state rides here rather than as props of the box**, because
 * the canvas mounts its cards by hand and a mount reads its props once: the
 * proxy is what makes a take starting under the reader redraw the bar, exactly
 * as a write to `draft` redraws the words.
 */
export interface Words {
  draft: string;
  busy: boolean;
  line: TakeLine | null;
  /** What this box's take row says, or `null` when the take is another box's. */
  phase: RowPhase | null;
  /** Whether the settings panel was opened from this box's mic. */
  panel: boolean;
  /** The record's take, and the counts this page produced for it. */
  take: Take | null;
  wire: LiveWire | null;
}

/** The comment box on a diff line: the spot it was opened on, and the words. */
export interface CommentBox extends Words {
  spot: ReviewSpot | null;
}

/** The reply box under a thread: which thread it answers, and the words. */
export interface ReplyBox extends Words {
  thread: string | null;
}

/** The counts this page's own capture produced, read live while a take runs. */
export interface LiveWire {
  frames: number;
  bytes: number;
  rate: number | null;
  dbfs: number[];
  elapsedMs: number;
}

/**
 * What dictation means to this page, shared by every box it draws: the one
 * take, the seat's axes and input, and the settings the panel edits them with.
 */
export interface Voice {
  dictate: Dictation;
  offered: boolean;
  connection: ComposerConnection | null;
  bind: Bind;
  mode: Mode;
  axes: DictateAxes;
  device: string | null;
  defaults: DictateAxes;
  onaxes: (axes: DictateAxes) => void;
  ondevice: (device: string | null) => void;
}
