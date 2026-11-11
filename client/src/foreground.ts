/**
 * The shell's resume, as one shared arm.
 *
 * **Android reports neither focus nor reliably a visibility change on
 * return**, so the shell emits `client://foreground` and that is the only
 * resume signal the phone gives. Three readers hang on it - the frame probe's
 * loop, the return re-read, and the board's marks beat - so the arm lives
 * here once rather than as a copy per reader.
 */

import { canHost } from './browser/host';

/**
 * Reports a shell resume as one event; returns its own disconnect, or null
 * where the page has no shell.
 *
 * **The Tauri API is imported lazily**, so the web build never loads it, and
 * a disconnect that arrives before the listener resolves takes it back on
 * resolution.
 */
export function observeForeground(cb: () => void): (() => void) | null {
  if (!canHost()) return null;
  let closed = false;
  let unlisten: (() => void) | null = null;
  void import('@tauri-apps/api/event')
    .then(({ listen }) => listen('client://foreground', cb))
    .then((stop) => {
      if (closed) stop();
      else unlisten = stop;
    })
    .catch((why: unknown) => {
      // **The arm is load-bearing on the phone**, so a failed install is said
      // aloud: a loop or a beat would stay stopped after a resume with no clue.
      console.warn('forge client: the shell foreground listener could not be installed', why);
    });
  return () => {
    closed = true;
    unlisten?.();
  };
}
