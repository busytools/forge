/**
 * The takeover, as the client draws it: the screen-replacing browser view a
 * hand-off's Open brings up.
 *
 * The state lives here rather than in the shell because the SCREEN is the
 * client's: the overlay, the bar and the way back are the web side's, and the
 * shell's engine only fills the area under the bar. `asking` is the hand-off
 * the bar's Done answers, armed by the dock while its prompt is the one up -
 * so the answer crosses from the bar exactly as it would from the dock.
 */

import { closeTakeover, openTakeover, takeoverState } from './host';

/** The hand-off the bar's Done would answer. */
export interface Asking {
  /** The hand-off's id, so the bar draws Done only while that one is up. */
  id: string;
  /** What the dock's own Done does, so the bar's Done is the same act. */
  done: () => void;
}

class Takeover {
  active = $state(false);
  asking = $state<Asking | null>(null);
  /**
   * Whether the picture is the browser's own view, rendered by the shell.
   * The screen then draws no frames and sizes nothing: the polling and the
   * canvas exist only for the platforms whose picture IS frames.
   */
  native = $state(false);

  /**
   * Open the view.
   *
   * Rejects with why when no engine is compiled into the shell; the dock
   * hears that and says the view could not be opened, rather than claiming a
   * browser this client does not have.
   */
  async open(): Promise<void> {
    await openTakeover();
    this.active = true;
    const state = await takeoverState();
    this.native = state.native;
  }

  /** Back out: the screen returns exactly as it was, and the dock keeps its question. */
  async back(): Promise<void> {
    this.active = false;
    await closeTakeover();
  }

  /** The bar's Done: the same answer the dock's Done gives, then back out.
   * The answer crosses first - it is the act; coming back out is the screen. */
  async done(): Promise<void> {
    const held = this.asking;
    this.asking = null;
    held?.done();
    await this.back();
  }

  /** What the shell says after a window reload: the view may still be up. */
  async sync(): Promise<void> {
    const state = await takeoverState();
    this.active = state.active;
    this.native = state.native;
  }
}

export const takeover = new Takeover();
