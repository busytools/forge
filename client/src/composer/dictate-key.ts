/**
 * The push-to-talk key, as a page reads it.
 *
 * Ported from `crates/forge-tui/src/app/dictate_key.rs`, which is the rule this
 * follows: the terminal is the reference implementation until it is deleted, so
 * the vocabulary, the tap window and every press/release decision below are the
 * terminal's rather than a second reading of the same words.
 *
 * Pure: the composer owns the window listener and the dispatch, and this owns
 * what a press means.
 */

/** The push-to-talk key, as `forge.toml` names it. */
export type Bind = 'right_cmd' | 'left_cmd' | 'off';

/** How a press and its release map onto starting and stopping a take. */
export type Mode = 'auto' | 'toggle' | 'hold';

/** What one press asks for, which is the terminal's own three. */
export type Action = 'begin' | 'finish' | 'cancel';

/**
 * A clean press-release shorter than this keeps recording; a longer one is a
 * hold whose release transcribes.
 */
export const TAP_MS = 300;

/** One press in flight, from its press to its release. */
export interface Held {
  /** When the press arrived, which is what the tap window measures. */
  since: number;
  /** Whether another key was pressed while this one was down. */
  chorded: boolean;
  /** Whether the take that is running was started by THIS press. */
  started: boolean;
}

/** What a key event asks for, and the press it leaves behind. */
export interface Step {
  held: Held | null;
  action: Action | null;
}

const NOTHING: Step = { held: null, action: null };

/**
 * The bind off a record, or the config's own default.
 *
 * The wire carries the closed vocabulary, so the only other value is an unborn
 * one - a record from a server that predates the field - and reading that as
 * `off` would silently disarm every install that never set a key of its own.
 */
export function bindFrom(value: unknown): Bind {
  return value === 'left_cmd' || value === 'off' ? value : 'right_cmd';
}

/** The mode off a record, or the config's own default, for the reason above. */
export function modeFrom(value: unknown): Mode {
  return value === 'toggle' || value === 'hold' ? value : 'auto';
}

/**
 * The `KeyboardEvent.code` the binding is delivered as, or `null` when no key
 * is bound.
 *
 * The browser names the modifier's own side, which is what lets a right-only
 * binding exist at all. Off macOS there is no Cmd key, so the cmd equivalent is
 * the right Control key - the substitution the terminal's own handler makes.
 */
export function boundCode(bind: Bind, mac: boolean): string | null {
  if (bind === 'off') return null;
  const left = bind === 'left_cmd';
  if (mac) return left ? 'MetaLeft' : 'MetaRight';
  return left ? 'ControlLeft' : 'ControlRight';
}

/** The bound key going down. */
export function down(held: Held | null, recording: boolean, at: number, mode: Mode): Step {
  // A second press, or a repeat, carries no new instruction: the press in
  // flight is the truth.
  if (held !== null) return { held, action: null };
  // In toggle mode the press IS the stop, so the live take ends here - nothing
  // is left tracked, because the release that follows has nothing to say.
  if (recording && mode === 'toggle') return { held: null, action: 'finish' };
  return {
    held: { since: at, chorded: false, started: !recording },
    action: recording ? null : 'begin',
  };
}

/** The bound key coming up. */
export function up(held: Held | null, at: number, mode: Mode): Step {
  // A release with no tracked press is a stray.
  if (held === null) return NOTHING;
  // Toggle ignores releases as stops entirely: the press already acted.
  if (mode === 'toggle') return NOTHING;
  if (held.chorded) return { held: null, action: held.started ? 'cancel' : null };
  if (!held.started) return { held: null, action: 'finish' };
  const heldLongEnough = mode === 'hold' || at - held.since >= TAP_MS;
  return { held: null, action: heldLongEnough ? 'finish' : null };
}

/** Mark the press in flight a chord, which any other key makes it. */
export function markChorded(held: Held | null): Held | null {
  return held === null ? null : { ...held, chorded: true };
}

/**
 * Whether an event is a bare modifier rather than a key.
 *
 * The terminal consumes every bare modifier without chording the hold, because
 * a modifier is not text and not a shortcut - and a chord here would discard
 * the take its press began, losing the reader's words to a key they brushed.
 */
export function isBareModifier(key: string): boolean {
  return (
    key === 'Alt' || key === 'AltGraph' || key === 'Control' || key === 'Meta' || key === 'Shift'
  );
}
