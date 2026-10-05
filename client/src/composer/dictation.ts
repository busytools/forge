/**
 * The dictation panel's own reads: the axes, the value in force on each, the
 * key hint, and what a chip asks the core for.
 *
 * Ported in VOCABULARY from the terminal's `/dictate` overlay
 * (`crates/forge-tui/src/app/dictate_picker.rs`) and drawn as the mockup draws
 * it rather than as the terminal's rows: the axes and their exact values are
 * the reference's, the form is the web's own.
 */

import type { Mode } from './dictate-key';
import { axesFrom, type DictateAxes } from '../session/wire';
import type { DictateContext, DictateStructure, DictateStyling } from '../session/wire';

/** One value an axis offers, with the label the panel draws for it. */
export interface Option<T extends string> {
  value: T;
  label: string;
}

/** What the take's cleanup may do, least to most formal. */
export const VOICE: readonly Option<DictateStyling>[] = [
  { value: 'casual', label: 'casual' },
  { value: 'semi_casual', label: 'semi-casual' },
  { value: 'semi_formal', label: 'semi-formal' },
  { value: 'formal', label: 'formal' },
];

/** Whether the model may turn enumerable content into a list. */
export const STRUCTURE: readonly Option<DictateStructure>[] = [
  { value: 'prose', label: 'prose' },
  { value: 'lists', label: 'may bullet a list' },
];

/** What the words are being written for. */
export const DESTINATION: readonly Option<DictateContext>[] = [
  { value: 'general', label: 'plain text' },
  { value: 'email', label: 'email layout' },
];

/** How a press of the push-to-talk key maps onto a take. */
export const MODES: readonly Option<Mode>[] = [
  { value: 'auto', label: 'auto' },
  { value: 'toggle', label: 'toggle' },
  { value: 'hold', label: 'hold' },
];

/** The axes a chip may set, which is every one the wire carries a command for. */
export type Axis = 'voice' | 'structure' | 'destination';

/** The Rust variant each panel axis crosses as, which is not its label. */
export const OVERRIDE: Readonly<Record<Axis, string>> = {
  voice: 'styling',
  structure: 'structure',
  destination: 'context',
};

/**
 * Where this client keeps what the reader chose on a seat: the axes its panel
 * is showing and the input it picked.
 *
 * Per seat, in the browser's own storage. The values are the CLIENT's now -
 * the greeting carries the defaults, and each take carries the values in
 * force - so a reload or a reconnect returns them from here rather than from
 * the server, and the panel's marks keep their meaning against the config
 * value as the default.
 */
function stored(key: string): unknown {
  try {
    const raw = globalThis.localStorage?.getItem(key);
    return raw === null || raw === undefined ? null : JSON.parse(raw);
  } catch {
    // Storage refused (private mode, a hardened browser): the defaults stand
    // for this session rather than the panel failing to draw.
    return null;
  }
}

function keep(key: string, value: unknown): void {
  try {
    if (value === null) globalThis.localStorage?.removeItem(key);
    else globalThis.localStorage?.setItem(key, JSON.stringify(value));
  } catch {
    // The choice stands for the session; nothing else can be done about it.
  }
}

const axesKey = (seat: string): string => `forge.dictate.axes.${seat}`;
const deviceKey = (seat: string): string => `forge.dictate.device.${seat}`;

/** What this seat dictates with: its own edits over the greeting's defaults. */
export function axesFor(seat: string, defaults: DictateAxes): DictateAxes {
  const held = stored(axesKey(seat));
  return held === null ? defaults : axesFrom(held);
}

/** Remember this seat's axes, or forget them (reset). */
export function rememberAxes(seat: string, axes: DictateAxes | null): void {
  keep(axesKey(seat), axes);
}

/** The input this seat records from, or `null` for the system default. */
export function deviceFor(seat: string): string | null {
  const held = stored(deviceKey(seat));
  return typeof held === 'string' ? held : null;
}

/** Remember this seat's input, or forget it (back to the system default). */
export function rememberDevice(seat: string, device: string | null): void {
  keep(deviceKey(seat), device);
}

/**
 * The key the panel advertises, or `null` when no key is bound.
 *
 * `null` is the whole point: `forge.toml` allows the binding to be `off`, and a
 * hint naming a key that will never fire the take is worse than no hint.
 */
export function keyHint(bind: string, mac: boolean): string | null {
  if (bind === 'off') return null;
  const side = bind === 'left_cmd' ? 'left' : 'right';
  return `${mac ? '\u{2318}' : 'Ctrl'} ${side} to talk`;
}
