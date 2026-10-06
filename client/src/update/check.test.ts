// @vitest-environment jsdom
import { invoke, isTauri } from '@tauri-apps/api/core';
import { beforeEach, describe, expect, it, vi } from 'vitest';

import { checkForUpdate, installUpdate, restartApp } from './check';

vi.mock('@tauri-apps/api/core', () => ({
  invoke: vi.fn(),
  isTauri: vi.fn(),
}));

const mockInvoke = vi.mocked(invoke);
const mockIsTauri = vi.mocked(isTauri);

beforeEach(() => {
  mockInvoke.mockReset();
  mockIsTauri.mockReset();
});

describe('checkForUpdate', () => {
  it('asks the shell for the version an update would install', async () => {
    mockIsTauri.mockReturnValue(true);
    mockInvoke.mockResolvedValue('1.0.116');

    await expect(checkForUpdate()).resolves.toBe('1.0.116');
    expect(mockInvoke).toHaveBeenCalledWith('check_update');
  });

  it('answers null when the build is current', async () => {
    mockIsTauri.mockReturnValue(true);
    mockInvoke.mockResolvedValue(null);

    await expect(checkForUpdate()).resolves.toBeNull();
  });

  it('answers null outside the shell without asking anything', async () => {
    mockIsTauri.mockReturnValue(false);

    await expect(checkForUpdate()).resolves.toBeNull();
    expect(mockInvoke).not.toHaveBeenCalled();
  });

  it('answers null when the check itself fails', async () => {
    mockIsTauri.mockReturnValue(true);
    mockInvoke.mockRejectedValue(new Error('offline'));

    await expect(checkForUpdate()).resolves.toBeNull();
  });
});

describe('installUpdate', () => {
  it('asks the shell to install the found update', async () => {
    mockInvoke.mockResolvedValue(undefined);

    await installUpdate();
    expect(mockInvoke).toHaveBeenCalledWith('install_update');
  });

  it('propagates a failed install for the surface to draw', async () => {
    mockInvoke.mockRejectedValue(new Error('the download failed'));

    await expect(installUpdate()).rejects.toThrow('the download failed');
  });
});

describe('restartApp', () => {
  it('asks the shell to restart into the installed update', async () => {
    mockInvoke.mockResolvedValue(undefined);

    await restartApp();
    expect(mockInvoke).toHaveBeenCalledWith('restart_app');
  });
});
