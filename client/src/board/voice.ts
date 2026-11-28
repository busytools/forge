/**
 * What dictation means to the board page, shared by every box it draws.
 *
 * **One microphone and one take per page**, the same rule the composer and
 * the review page keep: every box wears the same control, a take pressed in
 * one draws its bar in that box and nowhere else, and its words land back in
 * the box that began them. The board has no seat of its own, so the take
 * belongs to the project's lead - the seat whose board this is.
 */

import type { Dictation, RowPhase, takeCounts } from '../composer/dictate.svelte';
import type { Take } from '../composer/wire';

/** One text box the board draws, as the take's landing reaches it. */
export interface BoardFieldHandle {
  /** Put landed words in the box, replacing what is there. */
  set: (text: string) => void;
  /** Put the keyboard back in the box. */
  focus: () => void;
}

/** What the board page hands each of its text boxes. */
export interface BoardVoice {
  dictate: Dictation;
  /** The box whose mic began the running take, or `null` when none runs. */
  activeId: string | null;
  /** The record's take, once the server has one. */
  take: Take | null;
  phase: RowPhase | null;
  wire: ReturnType<typeof takeCounts>;
  silent: string | null;
  /** The line the last take attempt left, and the box it belongs to. */
  line: string | null;
  lineId: string | null;
  /** The box's mic was pressed: this take's words belong to it. */
  press: (id: string) => void;
  /** Cancel the running take. */
  cancel: () => void;
  /** Register a box's landing target; answer the unregister. */
  register: (id: string, box: BoardFieldHandle) => () => void;
}
