import { beforeEach, describe, expect, it, vi } from 'vitest';

import { latestPublished } from './web';

const mockFetch = vi.fn();

beforeEach(() => {
  vi.stubGlobal('fetch', mockFetch);
  mockFetch.mockReset();
});

describe('latestPublished', () => {
  it('reads the version the manifest beside the app names, from the root', async () => {
    mockFetch.mockResolvedValue({
      ok: true,
      json: () => Promise.resolve({ version: '1.0.117', platforms: {}, web: {} }),
    });

    await expect(latestPublished()).resolves.toBe('1.0.117');
    expect(mockFetch).toHaveBeenCalledWith('/latest.json');
    // Rooted, not relative: `./latest.json` from a session deep link would
    // resolve under the session's path, where the app's own fallback answers
    // 200 with the page - a silent null rather than an answer.
    const called: unknown = mockFetch.mock.calls[0]?.[0];
    expect(new URL(String(called), 'https://forge.hub/session/Org/Project/lead').pathname).toBe(
      '/latest.json',
    );
  });

  it('answers null when nothing serves a manifest', async () => {
    mockFetch.mockResolvedValue({ ok: false, json: () => Promise.resolve({}) });

    await expect(latestPublished()).resolves.toBeNull();
  });

  it('answers null when the fetch itself fails', async () => {
    mockFetch.mockRejectedValue(new Error('offline'));

    await expect(latestPublished()).resolves.toBeNull();
  });

  it('answers null when the manifest carries no version', async () => {
    mockFetch.mockResolvedValue({ ok: true, json: () => Promise.resolve({ platforms: {} }) });

    await expect(latestPublished()).resolves.toBeNull();
  });
});
