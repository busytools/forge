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
import type {
  DictateContext,
  DictateOverrides,
  DictateStructure,
  DictateStyling,
} from '../session/wire';

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

/** The value in force per axis, which is the crate's default where the session set none. */
export interface Force {
  styling: DictateStyling;
  structure: DictateStructure;
  context: DictateContext;
}

/**
 * What a take would actually use: the session's overrides over the crate's
 * defaults.
 *
 * The defaults are the normalizer's own (`forge-dictate`'s `#[default]`), so a
 * panel drawing them as the unset state draws what a take would do rather than
 * what the panel happens to prefer.
 */
export function inForce(overrides: DictateOverrides): Force {
  return {
    styling: overrides.styling ?? 'semi_formal',
    structure: overrides.structure ?? 'prose',
    context: overrides.context ?? 'general',
  };
}

/** The update one chip sends, as `DictateOverrideUpdate` serialises. */
export function pickUpdate(axis: Axis, value: string): Record<string, unknown> {
  return { [OVERRIDE[axis]]: value };
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

/** The input a pick moved the process to. */
export type DevicePick = { device: string } | 'system' | null;

/**
 * The pick the home snapshot carries, narrowed where the composer reads it.
 *
 * `null` means the `forge.toml` pin stands, which is a state rather than an
 * absence: the panel draws the config's own device for it. A shape nothing here
 * knows reads as `null` too, which is the same least-alarming reading the wire
 * takes everywhere else - the row then draws the pin, which is true and merely
 * not current.
 */
export function devicePick(value: unknown): DevicePick {
  if (value === 'system') return 'system';
  if (value !== null && typeof value === 'object') {
    const held = value as Record<string, unknown>;
    if (typeof held['device'] === 'string') return { device: held['device'] };
  }
  return null;
}
