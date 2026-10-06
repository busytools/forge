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

import { closeTakeover, openTakeover, takeoverActive } from './host';

/** How much of the top the bar takes, so the engine's view fills the rest. */
export const TAKEOVER_BAR_PX = 44;

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
   * Open the view.
   *
   * Rejects with why when no engine is compiled into the shell; the dock
   * hears that and falls back to the headed window it has always had, so a
   * failed takeover is a working browser rather than an empty screen.
   */
  async open(): Promise<void> {
    await openTakeover(TAKEOVER_BAR_PX);
    this.active = true;
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
    this.active = await takeoverActive();
  }
}

export const takeover = new Takeover();
