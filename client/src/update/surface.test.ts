// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { get } from 'svelte/store';
import { afterEach, describe, expect, it, vi } from 'vitest';

import { homeWire } from '../dev/fixture.data';
import Home from '../home/Home.svelte';
import { installUpdate, restartApp } from './check';
import { updateState } from './state';

vi.mock('./check', () => ({
  checkForUpdate: vi.fn(),
  installUpdate: vi.fn(),
  restartApp: vi.fn(),
}));

const mockInstall = vi.mocked(installUpdate);
const mockRestart = vi.mocked(restartApp);

afterEach(() => {
  updateState.set({ stage: 'current' });
});

describe("the home header's update line", () => {
  it('installs from the available line, and the restart line finishes it', async () => {
    updateState.set({ stage: 'available', version: '9.9.9' });
    mockInstall.mockResolvedValue(undefined);
    mockRestart.mockResolvedValue(undefined);

    const target = document.createElement('div');
    document.body.append(target);
    const app = mount(Home, { target, props: { wire: homeWire } });
    try {
      target.querySelector('button.upd')?.dispatchEvent(new MouseEvent('click', { bubbles: true }));
      flushSync();
      await vi.waitFor(() => expect(mockInstall).toHaveBeenCalledTimes(1));
      await vi.waitFor(() =>
        expect(get(updateState)).toEqual({ stage: 'restart', version: '9.9.9' }),
      );

      // The restart is the second half of the same control, on the line that
      // replaces the notice: nothing restarts until it is clicked.
      target.querySelector('button.upd')?.dispatchEvent(new MouseEvent('click', { bubbles: true }));
      flushSync();
      await vi.waitFor(() => expect(mockRestart).toHaveBeenCalledTimes(1));
    } finally {
      await unmount(app);
      target.remove();
    }
  });

  it('retries an install from the failed line, and nothing restarts', async () => {
    updateState.set({ stage: 'failed', version: '9.9.9', detail: 'the download failed' });
    mockInstall.mockResolvedValue(undefined);

    const target = document.createElement('div');
    document.body.append(target);
    const app = mount(Home, { target, props: { wire: homeWire } });
    try {
      target.querySelector('button.upd')?.dispatchEvent(new MouseEvent('click', { bubbles: true }));
      flushSync();
      await vi.waitFor(() => expect(mockInstall).toHaveBeenCalledTimes(1));
      expect(mockRestart, 'the retry restarted instead of installing again').not.toHaveBeenCalled();
    } finally {
      await unmount(app);
      target.remove();
    }
  });
});
