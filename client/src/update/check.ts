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

/** Download and install the found update. Restarting is a step of its own. */
export async function installUpdate(): Promise<void> {
  await invoke('install_update');
}

/** Restart into the installed update. */
export async function restartApp(): Promise<void> {
  await invoke('restart_app');
}
