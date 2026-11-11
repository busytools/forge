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
 * **`arm` names the reader in the failure warning**, because a phone's log is
 * the only place a failed install is ever seen and a bare "shell listener"
 * would not say which of the three stopped.
 *
 * **The Tauri API is imported lazily**, so the web build never loads it. A
 * disconnect that lands before the import resolves registers nothing; one
 * that lands after the listener is installed withdraws it.
 */
export function observeForeground(arm: string, cb: () => void): (() => void) | null {
  if (!canHost()) return null;
  let closed = false;
  let unlisten: (() => void) | null = null;
  void import('@tauri-apps/api/event')
    .then(({ listen }) => (closed ? null : listen('client://foreground', cb)))
    .then((stop) => {
      if (stop === null) return;
      if (closed) stop();
      else unlisten = stop;
    })
    .catch((why: unknown) => {
      // **The arm is load-bearing on the phone**, so a failed install is said
      // aloud: a loop or a beat would stay stopped after a resume with no clue.
      console.warn(`forge client: the ${arm} foreground listener could not be installed`, why);
    });
  return () => {
    closed = true;
    unlisten?.();
  };
}
