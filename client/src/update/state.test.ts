import { get } from 'svelte/store';
import { beforeEach, describe, expect, it, vi } from 'vitest';

import { checkForUpdate, installUpdate, restartApp } from './check';
import { install, restart, updateState, watchUpdate } from './state';

vi.mock('./check', () => ({
  checkForUpdate: vi.fn(),
  installUpdate: vi.fn(),
  restartApp: vi.fn(),
}));

const mockCheck = vi.mocked(checkForUpdate);
const mockInstall = vi.mocked(installUpdate);
const mockRestart = vi.mocked(restartApp);

beforeEach(() => {
  vi.resetAllMocks();
  updateState.set({ stage: 'current' });
});

describe('watchUpdate', () => {
  it('names the version an update would install', async () => {
    mockCheck.mockResolvedValue('1.0.116');

    await watchUpdate();

    expect(get(updateState)).toEqual({ stage: 'available', version: '1.0.116' });
  });

  it('stays current when the build is', async () => {
    mockCheck.mockResolvedValue(null);

    await watchUpdate();

    expect(get(updateState)).toEqual({ stage: 'current' });
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
