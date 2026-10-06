import { get, writable } from 'svelte/store';

import { checkForUpdate, installUpdate, restartApp } from './check';

/**
 * What the client's own update is doing, as the home header draws it: one
 * check at launch, the install a reader asks for, and the restart that
 * finishes it.
 *
 * `current` is also the unknown state - a build already current, and a check
 * that could not run - because neither is something the header can offer.
 *
 * `failed` carries the shell's own words for what went wrong, which the line
 * keeps on the control: a permanent failure and a transient one draw the same
 * sentence, and the reason is what tells a reader which they have.
 */
export type UpdateState =
  | { stage: 'current' }
  | { stage: 'available'; version: string }
  | { stage: 'installing'; version: string }
  | { stage: 'restart'; version: string }
  | { stage: 'failed'; version: string; detail: string };

export const updateState = writable<UpdateState>({ stage: 'current' });

/** The launch check. The shell calls this once; a call of its own checks again. */
export async function watchUpdate(): Promise<void> {
  const version = await checkForUpdate();
  if (version !== null) updateState.set({ stage: 'available', version });
}

/** Install the found update. Nothing restarts until `restart` is called. */
export async function install(): Promise<void> {
  const held = get(updateState);
  if (held.stage !== 'available' && held.stage !== 'failed') return;
  const { version } = held;
  updateState.set({ stage: 'installing', version });
  try {
    await installUpdate();
    updateState.set({ stage: 'restart', version });
  } catch (err) {
    updateState.set({
      stage: 'failed',
      version,
      detail: err instanceof Error ? err.message : String(err),
    });
  }
}

/** Restart into the installed update. */
export async function restart(): Promise<void> {
  await restartApp();
}
