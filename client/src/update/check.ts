import { invoke, isTauri } from '@tauri-apps/api/core';

/**
 * The version an update check found, or `null` when this build is current.
 *
 * Outside the shell there is nothing to update, and a check that cannot run is
 * not an update either: both answer `null` rather than raising a notice.
 */
export async function checkForUpdate(): Promise<string | null> {
  if (!isTauri()) return null;
  try {
    return await invoke<string | null>('check_update');
  } catch {
    return null;
  }
}

/**
 * What finishes an install: the desktop restarts into the swapped bundle,
 * the phone's install is the system prompt it was handed to.
 */
export type InstallStage = 'restart' | 'install';

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
