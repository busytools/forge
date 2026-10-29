import { isTauri } from '@tauri-apps/api/core';
import { get, writable } from 'svelte/store';

import { CLIENT_VERSION } from '../protocol';
import { checkForUpdate, installUpdate, restartApp } from './check';
import { isNewer } from './version';
import { latestPublished } from './web';

/**
 * What the client's own update is doing, as the home header draws it: one
 * check at launch, the tap a reader makes on the offer, and the stage that
 * finishes it.
 *
 * `checking` is where it starts: the launch check is a network fetch on the
 * phone, and a page that began on `current` would draw no line for however
 * long that fetch takes.
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
  | { stage: 'checking' }
  | { stage: 'current' }
  | { stage: 'available'; version: string }
  | { stage: 'installing'; version: string }
  | { stage: 'restart'; version: string }
  | { stage: 'failed'; version: string; detail: string }
  | { stage: 'check-failed'; detail: string }
  | { stage: 'web'; latest: string | null };

export const updateState = writable<UpdateState>({ stage: 'checking' });

/** The launch check. The shell calls this once; a call of its own checks again. */
export async function watchUpdate(): Promise<void> {
  if (isTauri()) {
    try {
      const version = await checkForUpdate();
      updateState.set(version === null ? { stage: 'current' } : { stage: 'available', version });
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

/**
 * Act on the found update, landing on the stage the shell says finishes it.
 * The phone's stage is the release page opening, which leaves the offer
 * itself unmoved: the line goes back to where it was and a second tap
 * reopens the page.
 */
export async function install(): Promise<void> {
  const held = get(updateState);
  if (held.stage !== 'available' && held.stage !== 'failed') {
    return;
  }
  const { version } = held;
  updateState.set({ stage: 'installing', version });
  try {
    const stage = await installUpdate();
    updateState.set(stage === 'browser' ? { stage: 'available', version } : { stage, version });
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
