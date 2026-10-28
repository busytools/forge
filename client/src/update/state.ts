import { isTauri } from '@tauri-apps/api/core';
import { get, writable } from 'svelte/store';

import { CLIENT_VERSION } from '../protocol';
import { checkForUpdate, installUpdate, restartApp } from './check';
import { isNewer } from './version';
import { latestPublished } from './web';

/**
 * What the client's own update is doing, as the home header draws it: one
 * check at launch, the install a reader asks for, and the restart that
 * finishes it.
 *
 * `current` is a build already current, and nothing else: a check that could
 * not run is its own stage, because "no update" and "could not check" must
 * not draw the same line.
 *
 * `failed` carries the shell's own words for what went wrong, which the line
 * keeps on the control: a permanent failure and a transient one draw the same
 * sentence, and the reason is what tells a reader which they have.
 * `check-failed` carries them for a launch check, where there is no version
 * to keep and nothing to retry.
 */
type UpdateState =
  | { stage: 'current' }
  | { stage: 'available'; version: string }
  | { stage: 'installing'; version: string }
  | { stage: 'restart'; version: string }
  | { stage: 'install'; version: string }
  | { stage: 'failed'; version: string; detail: string }
  | { stage: 'check-failed'; detail: string }
  | { stage: 'web'; latest: string | null };

export const updateState = writable<UpdateState>({ stage: 'current' });

/** The launch check. The shell calls this once; a call of its own checks again. */
export async function watchUpdate(): Promise<void> {
  if (isTauri()) {
    try {
      const version = await checkForUpdate();
      if (version !== null) updateState.set({ stage: 'available', version });
    } catch (err) {
      updateState.set({
        stage: 'check-failed',
        detail: err instanceof Error ? err.message : String(err),
      });
    }
    return;
  }
  // A browser installs nothing: what it can say is which build it is - the
  // line draws that from the build itself - and which release the app beside
  // it names, when that one is newer. The next load carries it.
  const published = await latestPublished();
  updateState.set({
    stage: 'web',
    latest: published !== null && isNewer(published, CLIENT_VERSION) ? published : null,
  });
}

/** Install the found update, landing on the stage the shell says finishes it. */
export async function install(): Promise<void> {
  const held = get(updateState);
  if (held.stage !== 'available' && held.stage !== 'failed' && held.stage !== 'install') {
    return;
  }
  const { version } = held;
  updateState.set({ stage: 'installing', version });
  try {
    const stage = await installUpdate();
    updateState.set({ stage, version });
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
