/**
 * One page's dictation: the take it owns, the presses that start and end it,
 * and the two cases where a press is not its own to act on.
 *
 * Lifted out of the composer so the same mic drives every surface that takes
 * typing. What a surface differs in is where a landed take's words go and
 * where a line about a take draws - both stay with the surface, because both
 * are its own box; what is shared is the microphone and the take's lifecycle.
 *
 * One per page, because the microphone is the client's own: a page that draws
 * a second mic - the composer's dock - drives this same take rather than
 * starting another.
 */

import { untrack } from 'svelte';

import type { DictateAxes } from '../session/wire';
import type { SessionSlot } from '../wire/types';
import { boxKey } from './box.svelte';
import {
  boundCode,
  down,
  isBareModifier,
  markChorded,
  up,
  type Action,
  type Bind,
  type Held,
  type Mode,
} from './dictate-key';
import { rememberDefaultDevice } from './dictation';
import { busyLine, LocalTake, type TakeConnection } from './take';

/** What registering the push-to-talk key needs of one surface. */
export interface PushToTalk {
  /** The take the key begins and ends, read where it is used: a surface's
   *  driver arrives as a prop, and a value taken once would be the one the
   *  page first drew with. */
  dictate: () => Dictation;
  /** Whether this install offers dictation at all. */
  offered: () => boolean;
  /** The push-to-talk key `forge.toml` configured. */
  bind: () => Bind;
  /** How a press of it maps onto a take, from the same section. */
  mode: () => Mode;
  /**
   * Where the keyboard goes when a press begins a take: the shortcut is how a
   * reader begins without the mouse, so the transcript lands where their keys
   * already are.
   */
  onBegin: () => void;
}

/**
 * The push-to-talk key, as one surface registers it.
 *
 * **Shared, so every surface that takes typing reads the key the same way**:
 * the configured binding, the mode's press-and-release rule, and the
 * install's own gate - a surface whose mic is not offered must not start
 * takes from the key either. Registered on the window, because the binding
 * is a bare modifier and arrives wherever the focus is; removed when the
 * surface goes.
 */
export function bindPushToTalk(wiring: PushToTalk): void {
  /** Whether this platform delivers Cmd, which is what a binding names. */
  const mac = navigator.platform.toLowerCase().includes('mac');
  /** The key's press in flight, from its press to its release. */
  let pressed: Held | null = null;

  $effect(() => {
    if (!wiring.offered()) return;
    const onDown = (event: KeyboardEvent): void => {
      // A held key repeats, and a repeat carries no instruction - without
      // this a toggle take stops once per repeat.
      if (event.repeat) return;
      if (event.code !== boundCode(wiring.bind(), mac)) {
        // Any other KEY while the press is down makes it a chord, and the
        // chord's release discards what the press began. A bare modifier is
        // not a key: those go through unchorded.
        if (!isBareModifier(event.key)) pressed = markChorded(pressed);
        return;
      }
      const step = down(pressed, wiring.dictate().held, Date.now(), wiring.mode());
      pressed = step.held;
      if (step.action === 'begin') wiring.onBegin();
      if (step.action !== null) wiring.dictate().act(step.action);
    };
    const onUp = (event: KeyboardEvent): void => {
      if (event.code !== boundCode(wiring.bind(), mac)) return;
      const step = up(pressed, Date.now(), wiring.mode());
      pressed = step.held;
      if (step.action !== null) wiring.dictate().act(step.action);
    };
    window.addEventListener('keydown', onDown, true);
    window.addEventListener('keyup', onUp);
    return () => {
      window.removeEventListener('keydown', onDown, true);
      window.removeEventListener('keyup', onUp);
    };
  });
}

/** A line a surface draws about a take, in its own row. */
export interface TakeLine {
  tone: string;
  text: string;
}

export interface DictationWiring {
  /** The socket a take rides, read at the press as the surface reads it. */
  connection: () => TakeConnection;
  /** The seat showing, which is the one a take is addressed to. */
  seat: () => SessionSlot;
  /** Whether the seat's record already shows a take this page did not start. */
  shown: () => boolean;
  /** The axes in force, read at the press rather than at the open. */
  options: () => DictateAxes;
  /** The input in force, or `null` for the system default. */
  device: () => string | null;
  /**
   * A take is being attempted: whatever line the last attempt left is stale,
   * so a retry does not open under it.
   */
  onStart: () => void;
  /**
   * A line the take itself owes, which belongs where the attempt was made: a
   * microphone that would not open, a release the socket never took.
   */
  onLine: (line: TakeLine) => void;
  /**
   * A line the press owes, which belongs where the reader is now: the
   * microphone is held by another take, or by one this page cannot see.
   */
  onRefused: (line: TakeLine) => void;
  /**
   * The take is over on this side: the counts it produced. A surface that
   * draws no counts of its own leaves this out.
   */
  onEnded?: (counts: { seat: string; frames: number; bytes: number; elapsedMs: number }) => void;
}

export class Dictation {
  /** The take this page started, if any. */
  take = $state<LocalTake | null>(null);
  /** Whether a start is waiting on the browser's permission round trip. */
  opening = $state(false);
  /** Whether the settings panel is showing, which the mic's hold opens. */
  panel = $state(false);
  /**
   * A release or a cancel that landed while the microphone was still
   * opening, applied the moment the take exists - the permission prompt is
   * the everyday case, and it is seconds long.
   */
  #pending: Action | null = null;
  readonly #wiring: DictationWiring;

  constructor(wiring: DictationWiring) {
    this.#wiring = wiring;
  }

  /** Whether the seat's record shows a take this page did not start. */
  get held(): boolean {
    return this.#wiring.shown();
  }

  /**
   * Open the microphone and start a take this client captures.
   *
   * Async, because the browser's permission round trip is: the presses that
   * land while it is open are held rather than opening a second stream. A
   * microphone that will not open never reaches the server - it is this
   * side's failure, and it draws as one - and the take itself is registered
   * by the server the moment it receives `dictate_stream`, so the frames
   * that follow always find it.
   */
  async begin(): Promise<void> {
    if (this.opening) return;
    const live = untrack(() => this.take);
    if (live !== null) {
      this.#wiring.onRefused({ tone: 'bad', text: busyLine(live.seat) });
      return;
    }
    this.#wiring.onStart();
    // A gesture from an earlier attempt has nothing to stop.
    this.#pending = null;
    this.opening = true;
    try {
      // The pick in force AT THE PRESS, read once: the panel stays live
      // under the permission prompt, so a pick changed while the open is in
      // flight must not turn the take the reader asked for into a learned
      // "default" - nor a default open into a pick's name.
      const picked = this.#wiring.device();
      const started = await LocalTake.begin({
        connection: this.#wiring.connection(),
        seat: this.#wiring.seat(),
        options: this.#wiring.options(),
        device: picked,
        onLine: (text) => this.#wiring.onLine({ tone: 'bad', text }),
        onEnded: () => {
          const finished = this.take;
          if (finished !== null) {
            this.#wiring.onEnded?.({
              seat: boxKey(finished.seat),
              frames: finished.wire.frames,
              bytes: finished.wire.bytes,
              // The clock freezes with the counts: the take is over, and a
              // duration that kept counting would be reading the wait instead.
              elapsedMs: finished.wire.elapsedMs,
            });
          }
          this.take = null;
        },
      });
      if (started === null) {
        this.#pending = null;
        return;
      }
      // A take with no pick records from the system default, and opening it
      // is the one moment the browser names what that default IS - the list
      // it offers carries no default mark.
      if (picked === null && started.resolved !== null && started.resolved.label !== '') {
        rememberDefaultDevice(started.resolved);
      }
      this.take = started;
      if (this.#pending !== null) {
        const held = this.#pending;
        this.#pending = null;
        started.stop(held === 'finish');
      }
    } finally {
      this.opening = false;
    }
  }

  /** What a bound key or an Escape asks for, the same three the view this client replaced had. */
  act(action: Action): void {
    if (action === 'begin') {
      void this.begin();
      return;
    }
    // A gesture during the open is HELD for the take it belongs to.
    if (this.opening) {
      this.#pending = action;
      return;
    }
    // A take this page is holding: the microphone is ours to let go of, and
    // the frames stop with it.
    const live = untrack(() => this.take);
    if (live !== null) {
      live.stop(action === 'finish');
      return;
    }
    // A take this page did NOT start, which the record still shows - a page
    // reloaded mid-take, or another view's own on the same seat. The row is
    // on this reader's screen, so the key means here what it means there.
    if (this.#wiring.shown()) {
      void this.#wiring.connection().dispatch({
        dictate_stop: { key: this.#wiring.seat(), submit: action === 'finish' },
      });
    }
  }

  /** What the mic's own press asks for: begin, or finish what is live. */
  toggle(): void {
    const live = untrack(() => this.take);
    // **A take on ANOTHER seat is not this row's to end.** The microphone is
    // this client's, but the take belongs to the seat it started on - so the
    // press is refused by name rather than silently stopping a take this
    // reader cannot see.
    if (live !== null && boxKey(live.seat) !== boxKey(this.#wiring.seat())) {
      this.#wiring.onRefused({ tone: 'bad', text: busyLine(live.seat) });
      return;
    }
    if (this.opening || live !== null || this.#wiring.shown()) {
      this.act('finish');
      return;
    }
    void this.begin();
  }
}
