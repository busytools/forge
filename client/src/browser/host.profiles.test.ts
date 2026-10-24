// @vitest-environment jsdom
import { describe, expect, it, vi } from 'vitest';

vi.mock('@tauri-apps/api/core', () => ({ invoke: vi.fn() }));

import { invoke } from '@tauri-apps/api/core';

import { listProfiles } from './host';

/// **A row the server sent is never dropped for being unreadable** (rule
/// 25): filtering it made the strip's count and empty line report a state
/// the server never sent.
describe('the profile list', () => {
  it('keeps every row the server sent, drawing the unreadable ones', async () => {
    Object.defineProperty(window, '__TAURI_INTERNALS__', { value: {}, configurable: true });
    vi.mocked(invoke).mockResolvedValue({
      ok: true,
      rows: [
        { something: 'this page does not know' },
        { name: 'hunt', owner: null, up: true, windowed: false, busy: false, closed: false },
      ],
    });

    const rows = await listProfiles(null);

    expect(rows, 'both rows are drawn').toHaveLength(2);
    expect(rows[0]?.unreadable, 'the unnamed one is marked unreadable').toBe(true);
    expect(rows[1]).toMatchObject({ name: 'hunt', up: true, unreadable: false });
  });
});
