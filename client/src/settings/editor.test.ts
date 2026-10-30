import { describe, expect, it } from 'vitest';

import {
  ENTRY_FIELDS,
  addEntry,
  currentValue,
  dirtyCount,
  editRequests,
  emptyDraft,
  removeEntry,
  restartKeys,
  setValue,
  undoRemove,
  uniqueName,
  valueOf,
} from './editor';
import { settingsWire } from '../dev/settings.fixture';

function key(path: string) {
  for (const area of settingsWire.areas) {
    const found = area.keys.find((candidate) => candidate.path === path);
    if (found !== undefined) return found;
  }
  throw new Error(`the fixture has no ${path}`);
}

describe('the settings editor', () => {
  it('reads a value into the shape its control edits', () => {
    expect(valueOf({ kind: 'string', text: 'x' })).toBe('x');
    expect(valueOf({ kind: 'number', text: '8790' })).toBe(8790);
    expect(valueOf({ kind: 'bool', text: 'true' })).toBe(true);
    expect(valueOf({ kind: 'list', items: ['a'] })).toEqual(['a']);
    expect(valueOf({ kind: 'unset' })).toBeNull();
  });

  it('drops an edit that lands back on the loaded value', () => {
    const draft = emptyDraft();
    const disabled = key('server.first_frame_cap_kb');
    setValue(draft, disabled, 1024);
    expect(dirtyCount(draft), 'a changed value counts').toBe(1);
    setValue(draft, disabled, 512);
    expect(dirtyCount(draft), 'the loaded value again is not a change').toBe(0);
    expect(currentValue(draft, disabled), 'and the row draws the loaded value').toBe(512);
  });

  it('builds one set request per edited key, with its pattern and area', () => {
    const draft = emptyDraft();
    setValue(draft, key('server.port'), 8791);
    const requests = editRequests(draft, settingsWire);
    expect(requests).toEqual([
      {
        area: 'server',
        keyPath: 'server.port',
        pattern: 'server.port',
        op: 'set',
        value: 8791,
      },
    ]);
  });

  it('adds a skeleton entry with its required fields empty', () => {
    const draft = emptyDraft();
    const added = addEntry(settingsWire, draft, 'accounts', 'account', null);
    expect(added?.path, 'the entry is named under accounts').toBe('accounts.new-account');
    expect(added?.fields.display_name, 'a required field starts empty').toBeNull();
    expect(added?.fields.models, 'a list starts as an empty list').toEqual([]);
    expect(
      ENTRY_FIELDS.account?.some((field) => field.required),
      'the skeleton marks required',
    ).toBe(true);
    const requests = editRequests(draft, settingsWire);
    expect(requests[0]?.op).toBe('add');
    expect(requests[0]?.keyPath).toBe('accounts.new-account');
  });

  it('names a new account apart from the ones already there', () => {
    const draft = emptyDraft();
    const first = uniqueName(settingsWire, draft, 'accounts.', 'new-account');
    expect(first).toBe('new-account');
    addEntry(settingsWire, draft, 'accounts', 'account', null);
    const second = uniqueName(settingsWire, draft, 'accounts.', 'new-account');
    expect(second, 'a second add does not collide with the first').toBe('new-account-2');
  });

  it('removes an entry, and undoes it', () => {
    const draft = emptyDraft();
    removeEntry(draft, 'accounts', 'account', 'accounts.Granite', 'Granite');
    expect(dirtyCount(draft)).toBe(1);
    expect(editRequests(draft, settingsWire)[0]).toEqual({
      area: 'accounts',
      keyPath: 'accounts.Granite',
      pattern: '',
      op: 'remove',
    });
    undoRemove(draft, 'accounts.Granite');
    expect(dirtyCount(draft)).toBe(0);
  });

  it('lists the restart-class keys a save landed', () => {
    const draft = emptyDraft();
    setValue(draft, key('server.port'), 8791);
    setValue(draft, key('server.first_frame_cap_kb'), 1024);
    const requests = editRequests(draft, settingsWire);
    const after = {
      ...settingsWire,
      fingerprint: 'ffffffffffffffff',
      areas: settingsWire.areas.map((area) => ({
        ...area,
        keys: area.keys.map((held) =>
          held.path === 'server.port'
            ? { ...held, value: { kind: 'number' as const, text: '8791' } }
            : held.path === 'server.first_frame_cap_kb'
              ? { ...held, value: { kind: 'number' as const, text: '1024' } }
              : held,
        ),
      })),
    };
    expect(restartKeys(settingsWire, after, requests)).toEqual(['server.port']);
  });

  it("lists a removed entry's restart keys from the document it left", () => {
    const draft = emptyDraft();
    removeEntry(draft, 'server', 'server', 'server', 'server');
    const requests = editRequests(draft, settingsWire);
    expect(restartKeys(settingsWire, settingsWire, requests)).toContain('server.port');
  });
});
