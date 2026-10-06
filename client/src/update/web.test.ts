import { beforeEach, describe, expect, it, vi } from 'vitest';

import { latestPublished } from './web';

const mockFetch = vi.fn();

beforeEach(() => {
  vi.stubGlobal('fetch', mockFetch);
  mockFetch.mockReset();
});

describe('latestPublished', () => {
  it('reads the version the manifest beside the app names', async () => {
    mockFetch.mockResolvedValue({
      ok: true,
      json: () => Promise.resolve({ version: '1.0.117', platforms: {}, web: {} }),
    });

    await expect(latestPublished()).resolves.toBe('1.0.117');
    expect(mockFetch).toHaveBeenCalledWith('./latest.json');
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
