import { describe, expect, it } from 'vitest';

import { tabRows } from './host';

describe("the strip's tab rows", () => {
  it("names the owner and the state in the server's words", () => {
    const rows = tabRows({
      profile: 'Busytools/work',
      id: 3,
      up: true,
      connected: true,
      closing_at_ms: null,
      rows: [
        {
          number: 1,
          title: 'portal',
          url: 'https://x/',
          owner: { org: 'Busytools', project: 'forge', label: 'lead' },
          state: 'held',
        },
        {
          number: 2,
          title: '',
          url: 'about:blank',
          owner: { org: 'Busytools', project: 'forge', label: 'w1' },
          state: 'in_use',
        },
      ],
    });
    expect(rows[0]?.stateText).toBe('held');
    expect(rows[1]?.title).toBe('about:blank');
  });

  it('refuses a row with no owner', () => {
    expect(() =>
      tabRows({
        profile: 'p',
        id: 1,
        up: true,
        connected: false,
        closing_at_ms: null,
        rows: [{ number: 1, title: '', url: '', owner: null, state: 'idle' }],
      } as never),
    ).toThrow(/owner/);
  });
});
