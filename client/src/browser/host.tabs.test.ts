import { describe, expect, it } from 'vitest';

import { tabListOf, tabRows } from './host';

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

  /// **An unreadable list is a refusal, not an empty strip.** The rows used
  /// to be cast, so a malformed `rows` drew "no tabs yet" - a claim that a
  /// read found nothing.
  it('refuses a list whose rows this page cannot read', () => {
    expect(() =>
      tabListOf({ profile: 'p', id: 1, up: true, connected: true, rows: 'not-a-list' }),
    ).toThrow(/rows/);
    expect(() =>
      tabListOf({
        profile: 'p',
        id: 1,
        up: true,
        connected: true,
        rows: [{ number: 1, title: '', url: '', owner: { org: 'O', project: 'p', label: 'l' } }],
      }),
    ).toThrow(/state/);
  });

  /// **A host's state is read, not guessed.** Coercing a missing or
  /// malformed value to false drew "down" over a list that never said so.
  it('refuses a list that does not say whether its host is up', () => {
    expect(() => tabListOf({ profile: 'p', id: 1, up: true, rows: [] })).toThrow(
      /whether its host is up/,
    );
    expect(() =>
      tabListOf({ profile: 'p', id: 1, up: true, connected: 'yes', rows: [] }),
    ).toThrow(/whether its host is up/);
  });

  /// **A state this page has not seen reads as itself**, including a word
  /// that collides with an object property: the lookup is the map's own, not
  /// whatever `Object.prototype` carries.
  it('draws an unknown state as its own word', () => {
    const rows = tabRows({
      profile: 'p',
      id: 1,
      up: true,
      connected: true,
      closing_at_ms: null,
      rows: [
        {
          number: 1,
          title: '',
          url: '',
          owner: { org: 'O', project: 'p', label: 'l' },
          state: 'constructor',
        },
      ],
    });
    expect(rows[0]?.stateText).toBe('constructor');
  });
});
