import { invoke, isTauri } from '@tauri-apps/api/core';

/**
 * The version an update check found, or `null` when this build is current.
 *
 * Outside the shell there is nothing to update, so a browser answers `null`
 * without asking. A check that could not run is not a current build - the
 * shell's words are raised so the header can draw them, because "no update"
 * and "could not check" must not draw the same line.
 */
export async function checkForUpdate(): Promise<string | null> {
  if (!isTauri()) return null;
  return await invoke<string | null>('check_update');
}

/**
 * What finishes an install: the desktop restarts into the swapped bundle,
 * the phone's install is the system prompt it was handed to.
 */
type InstallStage = 'restart' | 'install';

/**
 * Download and install the found update, answering with the stage that
 * finishes it. This is the one place the wire's value narrows, and it refuses
 * rather than guessing: a stage with no words would otherwise become a
 * restart whose control the platform may not even answer.
 */
export async function installUpdate(): Promise<InstallStage> {
  const stage = await invoke<string>('install_update');
  if (stage !== 'restart' && stage !== 'install') {
    throw new Error(`the shell answered an unknown update stage: ${stage}`);
  }
  return stage;
}

/** Restart into the installed update. */
export async function restartApp(): Promise<void> {
  await invoke('restart_app');
}
