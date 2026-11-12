// @vitest-environment jsdom
/**
 * The share under a shell: the two doors into the same sheet.
 *
 * **A warm share is not a DOM event.** A cold start's share is the activity's
 * launch intent, which the page asks for once; a share into a running app is
 * the shell's `client://share` event. Both are driven here through the shell's
 * own APIs, because the failure this file pins is a share that never reaches
 * the store - a silent swallow that no page-level test can see.
 */
import { get } from 'svelte/store';
import { afterEach, describe, expect, it, vi } from 'vitest';

const { invoke, listeners } = vi.hoisted(() => ({
  invoke: vi.fn(),
  listeners: new Map<string, (event: unknown) => void>(),
}));

vi.mock('@tauri-apps/api/core', () => ({
  invoke,
  isTauri: () => true,
}));

vi.mock('@tauri-apps/api/event', () => ({
  listen: (name: string, cb: (event: unknown) => void) => {
    listeners.set(name, cb);
    return Promise.resolve(() => listeners.delete(name));
  },
}));

import { currentRoute, goTo, holdRoute } from '../routes';
import { launchShare, pendingShare, watchShares } from './pending';

const globals = window as unknown as Record<string, unknown>;

/** A payload as the shell's Android half answers it. */
function aShare(name: string): unknown {
  return {
    files: [{ path: `/cache/share/${name}`, name, size: 4, mime: 'image/jpeg' }],
  };
}

async function push(payload: unknown): Promise<void> {
  await watchShares();
  await vi.waitFor(() => expect(listeners.has('client://share')).toBe(true));
  listeners.get('client://share')?.({ payload });
}

afterEach(() => {
  vi.restoreAllMocks();
  invoke.mockReset();
  listeners.clear();
  pendingShare.set(null);
  currentRoute.set({ name: 'home' });
  delete globals['__TAURI_INTERNALS__'];
});

describe('a share into a running app', () => {
  it('opens the sheet on the share the shell pushes', async () => {
    globals['__TAURI_INTERNALS__'] = {};
    await push(aShare('a.jpg'));

    expect(get(pendingShare)?.files.map((file) => file.name)).toEqual(['a.jpg']);
    expect(get(currentRoute).name).toBe('share');
  });

  it('replaces an open sheet in place rather than pushing a second entry', async () => {
    globals['__TAURI_INTERNALS__'] = {};
    goTo({ name: 'share' });
    const pushed = vi.spyOn(history, 'pushState');

    await push(aShare('b.jpg'));

    expect(get(pendingShare)?.files.map((file) => file.name)).toEqual(['b.jpg']);
    expect(
      pushed,
      'a second /share entry would make the first Back stop on it',
    ).not.toHaveBeenCalled();
  });

  it('opens the sheet past a page hold rather than being swallowed', async () => {
    globals['__TAURI_INTERNALS__'] = {};
    const release = holdRoute(() => false);
    try {
      await push(aShare('c.jpg'));
    } finally {
      release();
    }
    expect(get(pendingShare)?.files.map((file) => file.name)).toEqual(['c.jpg']);
    expect(get(currentRoute).name).toBe('share');
  });

  it('ignores a payload that is not a share at all', async () => {
    globals['__TAURI_INTERNALS__'] = {};
    await push({ files: [] });
    expect(get(pendingShare)).toBeNull();
    expect(get(currentRoute).name).toBe('home');
  });

  it('unregisters its listener on the returned stop', async () => {
    globals['__TAURI_INTERNALS__'] = {};
    const stop = await watchShares();
    await vi.waitFor(() => expect(listeners.has('client://share')).toBe(true));
    stop();
    expect(listeners.has('client://share')).toBe(false);
  });
});

describe('a share this launch came from', () => {
  it('hands the cold-start share to the sheet', async () => {
    globals['__TAURI_INTERNALS__'] = {};
    invoke.mockResolvedValueOnce(aShare('cold.jpg'));

    expect(await launchShare()).toBe(true);
    expect(get(pendingShare)?.files.map((file) => file.name)).toEqual(['cold.jpg']);
  });

  it('is every launch that shared nothing, and opens nothing', async () => {
    globals['__TAURI_INTERNALS__'] = {};
    invoke.mockResolvedValueOnce({ files: [] });

    expect(await launchShare()).toBe(false);
    expect(get(pendingShare)).toBeNull();
  });

  it('survives a launch whose take fails', async () => {
    globals['__TAURI_INTERNALS__'] = {};
    invoke.mockRejectedValueOnce('the intake is not ready');
    vi.spyOn(console, 'warn').mockImplementation(() => {});

    expect(await launchShare()).toBe(false);
    expect(get(pendingShare)).toBeNull();
  });
});
