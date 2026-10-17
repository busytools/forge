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

import { isTauri } from '@tauri-apps/api/core';
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
import { Microphone, micSilence, silenceCause, type MicSilence } from './mic';
import { openShellMic } from './mic-shell';
import { busyLine, LocalTake, type MicSource, type TakeConnection } from './take';
import type { Take } from './wire';

/**
 * How this client opens a microphone, on the platform it runs on.
 *
 * The desktop's shell captures natively and hands the frames back, so a press
 * captures from the press; in a browser it is the webview's own stream, cold
 * start and all. One definition, so every surface that takes typing opens the
 * device the same way; everything after the open is one path.
 */
export function openCapture(device: string | null): Promise<MicSource> {
  return isTauri() ? openShellMic(device) : Microphone.open(device);
}

/** What registering the push-to-talk key needs of one surface. */
interface PushToTalk {
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

/**
 * How long a take may produce nothing before its row says so.
 *
 * Long enough that a reader pressing and drawing breath is not called silent,
 * short enough that a device feeding the capture nothing is named while it is
 * happening rather than only at the take's end.
 */
export const SILENT_NOTICE_MS = 3_000;

/** What the row says a capture it is hearing nothing from is doing. */
const SILENT_HEAD = 'the capture is open and nothing is coming through it';

/** The check the row falls back to when the platform named no cause. */
const SILENT_HINT = 'check the microphone is not muted, or held by another app';

/** A line a surface draws about a take, in its own row. */
export interface TakeLine {
  tone: string;
  text: string;
}

/**
 * What a take's row says it is doing, or `null` when there is no row.
 *
 * `ready` is a capture that is open and has produced nothing yet - a device
 * waking, a graph starting - and `recording` begins at the first frame
 * carrying a real sample, which is the moment the take is actually keeping a
 * voice (#2159). The record's own `transcribing` is the release, and it wins
 * over this page's take: the server finalised it before the page let go.
 *
 * **Shared, so every box that draws a take row says the same thing**: the
 * composer's own take line, the dock's words row, and the boxes that take a
 * take of their own. Whatever is on screen, the reader sees one reading.
 */
export type RowPhase = 'ready' | 'recording' | 'transcribing';

export function takePhase(
  dictate: Dictation,
  shown: Take | null,
  seat: SessionSlot,
): RowPhase | null {
  if (shown?.phase === 'transcribing') return 'transcribing';
  const live = dictate.take;
  if (live !== null && boxKey(live.seat) === boxKey(seat)) {
    return live.wire.flowing ? 'recording' : 'ready';
  }
  return shown === null ? null : 'recording';
}

/**
 * The counts the row draws, for a take THIS page is capturing on this seat.
 *
 * `null` for a take the record shows but this page did not produce: the counts
 * belong to the side that made the frames, and over a seat this page is not
 * showing there is nothing of its own to read.
 */
export function takeCounts(dictate: Dictation, seat: SessionSlot): LocalTake['wire'] | null {
  const live = dictate.take;
  if (live === null || boxKey(live.seat) !== boxKey(seat)) return null;
  return live.wire;
}

/**
 * The line a take owes when the device is feeding it nothing, or `null`.
 *
 * A capture that is open and producing digital silence - a per-app mic mode,
 * a privacy toggle, another app on the microphone - reads `ready` for the
 * whole take and draws no graph, so the reader used to learn it only once the
 * take ended. This is the row saying so while it is happening, withdrawn the
 * moment a sample lands because the take's own `flowing` is what decides it.
 * Shared, so every box that draws a take row can say the same thing.
 */
export function takeSilenceLine(dictate: Dictation, seat: SessionSlot): TakeLine | null {
  const live = dictate.take;
  if (live === null || boxKey(live.seat) !== boxKey(seat)) return null;
  if (!dictate.nothingHeard || live.wire.flowing) return null;
  const cause = silenceCause(dictate.silence);
  return { tone: 'q', text: `${SILENT_HEAD} \u{b7} ${cause ?? SILENT_HINT}` };
}

interface DictationWiring {
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
   * How this client opens a microphone: the browser's own stream, or the
   * desktop shell's native capture. The surface states which, because the
   * choice is the platform's rather than the take's.
   */
  capture: (device: string | null) => Promise<MicSource>;
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
   * Whether the live take has run for {@link SILENT_NOTICE_MS} with no sample.
   *
   * What a device feeding the capture nothing looks like from here, and the
   * state the row's line is drawn from - read together with the take's own
   * `flowing`, so a sample landing after this was set withdraws the line
   * rather than this clearing a flag nobody sees.
   */
  nothingHeard = $state(false);
  /**
   * What the platform says is wrong with the capture, read when the row's
   * line needs a cause; `null` until a read answers, and on every platform
   * with no reader.
   */
  silence = $state<MicSilence | null>(null);
  /**
   * A release or a cancel that landed while the microphone was still
   * opening, applied the moment the take exists - the permission prompt is
   * the everyday case, and it is seconds long.
   */
  #pending: Action | null = null;
  /** The bound's own timer, cleared with the take that set it. */
  #quiet: ReturnType<typeof setTimeout> | null = null;
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
      // **A take this seat is still closing is absorbed, not refused.** Its
      // release is draining the capture for a moment - up to a fifth of a
      // second (#2159) - and a press inside that window has nothing to start
      // and nothing to stop: the take it would name is the one the reader
      // just let go of. A take on another seat is the one worth naming.
      if (live.closing && boxKey(live.seat) === boxKey(this.#wiring.seat())) return;
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
        capture: this.#wiring.capture,
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
          this.#clearQuiet();
        },
      });
      if (started === null) {
        this.#pending = null;
        return;
      }
      // A take with no pick records from the system default, and opening it
      // is the one moment the take names what that default IS - the list it
      // offers carries no default mark, on either leg.
      if (picked === null && started.resolved !== null && started.resolved.label !== '') {
        rememberDefaultDevice(started.resolved);
      }
      this.take = started;
      // The bound is a clock rather than a frame count: a capture the device
      // is feeding nothing is exactly the one whose frames do not arrive to
      // re-render anything, so the row would never learn.
      this.#clearQuiet();
      this.nothingHeard = false;
      this.silence = null;
      this.#quiet = setTimeout(() => {
        // Not a take that is already ending: a release drains the capture for
        // a moment with the take still live and no sample in it, and the bound
        // landing there would flash the sentence over a take that is over.
        if (this.take !== null && !this.take.closing && !this.take.wire.flowing) {
          this.nothingHeard = true;
          void this.#readSilence();
        }
      }, SILENT_NOTICE_MS);
      if (this.#pending !== null) {
        const held = this.#pending;
        this.#pending = null;
        started.stop(held === 'finish');
      }
    } finally {
      this.opening = false;
    }
  }

  /** Drop the bound's timer, which belongs to the take that set it. */
  #clearQuiet(): void {
    if (this.#quiet !== null) clearTimeout(this.#quiet);
    this.#quiet = null;
  }

  /**
   * Ask the platform what is wrong with the capture, for the row's cause.
   * Fire and forget: the line draws without a cause if nothing answers, and
   * an answer landing after the take ended is dropped rather than kept.
   */
  async #readSilence(): Promise<void> {
    let state: MicSilence | null = null;
    try {
      state = await micSilence();
    } catch {
      // A reader that cannot answer names no cause: the line is owed anyway,
      // and the cause is the embellishment on it.
    }
    if (this.take !== null) this.silence = state;
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
