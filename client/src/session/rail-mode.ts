/**
 * How the projects rail is present: `static` - the column, always there -
 * or `hover`, the peek the chip summons.
 *
 * Remembered per device rather than in `forge.toml`: a desktop wants the
 * column and a phone does not, and the two read the same config. The default
 * is the column; the sheet folds a column to the peek's overlay on its own
 * below the width a column stops making sense at, whatever this says.
 */
export type RailMode = 'static' | 'hover';

const KEY = 'forge.rail';

export function rememberedRailMode(): RailMode {
  try {
    const raw = globalThis.localStorage?.getItem(KEY);
    // Anything but a current mode - including a `closed` from before the
    // close went - reads as the default.
    return raw === 'hover' ? 'hover' : 'static';
  } catch {
    return 'static';
  }
}

export function rememberRailMode(mode: RailMode): void {
  try {
    globalThis.localStorage?.setItem(KEY, mode);
  } catch {
    // A blocked store is a preference that does not persist, not a failure.
  }
}
