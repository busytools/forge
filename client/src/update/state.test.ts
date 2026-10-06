import { isTauri } from '@tauri-apps/api/core';
import { get } from 'svelte/store';
import { beforeEach, describe, expect, it, vi } from 'vitest';

import { CLIENT_VERSION } from '../protocol';
import { checkForUpdate, installUpdate, restartApp } from './check';
import { install, restart, updateState, watchUpdate } from './state';
import { latestPublished } from './web';
import { versionParts } from './version';

vi.mock('./check', () => ({
  checkForUpdate: vi.fn(),
  installUpdate: vi.fn(),
  restartApp: vi.fn(),
}));

vi.mock('./web', () => ({
  latestPublished: vi.fn(),
}));

vi.mock('@tauri-apps/api/core', () => ({
  isTauri: vi.fn(),
}));

const mockCheck = vi.mocked(checkForUpdate);
const mockInstall = vi.mocked(installUpdate);
const mockRestart = vi.mocked(restartApp);
const mockPublished = vi.mocked(latestPublished);
const mockIsTauri = vi.mocked(isTauri);

/** The release after the one this build is, so the test outlives a bump. */
function nextRelease(): string {
  const parts = versionParts(CLIENT_VERSION);
  if (parts === null) throw new Error('the build names no version');
  const [major, minor, patch] = parts;
  return `${major}.${minor}.${(patch ?? 0) + 1}`;
}

beforeEach(() => {
  vi.resetAllMocks();
  updateState.set({ stage: 'current' });
});

describe('watchUpdate', () => {
  it('names the version an update would install, in a shell', async () => {
    mockIsTauri.mockReturnValue(true);
    mockCheck.mockResolvedValue('1.0.116');

    await watchUpdate();

    expect(get(updateState)).toEqual({ stage: 'available', version: '1.0.116' });
  });

  it('stays current when the build is', async () => {
    mockIsTauri.mockReturnValue(true);
    mockCheck.mockResolvedValue(null);

    await watchUpdate();

    expect(get(updateState)).toEqual({ stage: 'current' });
  });

  /**
   * A browser tab installs nothing: it draws which build it is and, when the
   * app it was served with carries a manifest naming a newer release, that
   * release - which is only a fact, because the next load carries it.
   */
  it('names the published release outside a shell, when it is newer', async () => {
    mockIsTauri.mockReturnValue(false);
    mockPublished.mockResolvedValue(nextRelease());

    await watchUpdate();

    expect(get(updateState)).toEqual({ stage: 'web', latest: nextRelease() });
    expect(mockCheck, 'a browser reached a shell command').not.toHaveBeenCalled();
  });

  it('names no release when the published one is not newer', async () => {
    mockIsTauri.mockReturnValue(false);
    mockPublished.mockResolvedValue(CLIENT_VERSION);

    await watchUpdate();

    expect(get(updateState)).toEqual({ stage: 'web', latest: null });
  });

  it('names no release when nothing served a manifest', async () => {
    mockIsTauri.mockReturnValue(false);
    mockPublished.mockResolvedValue(null);

    await watchUpdate();

    expect(get(updateState)).toEqual({ stage: 'web', latest: null });
  });
});

describe('install', () => {
  it('holds the installing stage until the download lands, then offers the restart', async () => {
    updateState.set({ stage: 'available', version: '1.0.116' });
    let land = (): void => {};
    mockInstall.mockImplementation(
      () =>
        new Promise<'restart'>((resolve) => {
          land = () => resolve('restart');
        }),
    );

    const installing = install();
    expect(get(updateState)).toEqual({ stage: 'installing', version: '1.0.116' });

    land();
    await installing;
    expect(get(updateState)).toEqual({ stage: 'restart', version: '1.0.116' });
  });

  it("offers the installer when the phone's shell answers with it", async () => {
    updateState.set({ stage: 'available', version: '1.0.116' });
    mockInstall.mockResolvedValue('install');

    await install();

    expect(get(updateState)).toEqual({ stage: 'install', version: '1.0.116' });
  });

  it('draws the failure with its reason, keeping the version for a retry', async () => {
    updateState.set({ stage: 'available', version: '1.0.116' });
    mockInstall.mockRejectedValue(new Error('the download failed'));

    await install();

    expect(get(updateState)).toEqual({
      stage: 'failed',
      version: '1.0.116',
      detail: 'the download failed',
    });
  });

  it('carries a non-Error rejection the same way', async () => {
    updateState.set({ stage: 'available', version: '1.0.116' });
    mockInstall.mockRejectedValue('the signature did not match');

    await install();

    expect(get(updateState)).toEqual({
      stage: 'failed',
      version: '1.0.116',
      detail: 'the signature did not match',
    });
  });

  it('retries from a failure', async () => {
    updateState.set({ stage: 'failed', version: '1.0.116', detail: 'the download failed' });
    mockInstall.mockResolvedValue('restart');

    await install();

    expect(get(updateState)).toEqual({ stage: 'restart', version: '1.0.116' });
  });

  it('ignores a second call while one is in flight', async () => {
    updateState.set({ stage: 'available', version: '1.0.116' });
    const lands: Array<() => void> = [];
    mockInstall.mockImplementation(
      () =>
        new Promise<'restart'>((resolve) => {
          lands.push(() => resolve('restart'));
        }),
    );

    const first = install();
    const second = install();
    for (const land of lands) land();
    await Promise.all([first, second]);

    expect(mockInstall).toHaveBeenCalledTimes(1);
  });
});

describe('restart', () => {
  it('asks the shell to restart into the installed update', async () => {
    mockRestart.mockResolvedValue(undefined);

    await restart();

    expect(mockRestart).toHaveBeenCalledTimes(1);
  });
});
