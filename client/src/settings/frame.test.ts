import { get } from 'svelte/store';
import { describe, expect, it } from 'vitest';

import { ownSettingsFrame, settingsFrame, takeSettings } from './frame';

/** A one-key document, so a test can tell two frames apart by their value. */
function document(mark: string): unknown {
  return {
    version: 'v',
    source: '~/.claude/forge/forge.toml',
    totals: { keys: 1, areas: 1 },
    areas: [
      {
        id: 'client',
        name: 'client',
        note: '',
        keys: [{ path: 'client.mark', kind: 'string', value: mark, class: 'live' }],
      },
    ],
  };
}

describe('the settings frame store', () => {
  it("draws only the taken connection's frame", () => {
    const launch = {};
    const person = {};
    takeSettings(document('klin'), launch);
    ownSettingsFrame(person);
    expect(get(settingsFrame).status, 'a frame from a replaced launch is not drawn').toBe(
      'waiting',
    );

    takeSettings(document('klin'), launch);
    expect(get(settingsFrame).status, 'and a late one is still ignored').toBe('waiting');

    takeSettings(document('stencil_f'), person);
    const frame = get(settingsFrame);
    expect(frame.status).toBe('ready');
    if (frame.status !== 'ready') throw new Error("the person's frame drew");
    expect(frame.document.areas[0]?.keys[0]?.value).toEqual({
      kind: 'string',
      text: 'stencil_f',
    });
  });

  it('keeps a frame that landed before the connection was taken', () => {
    const person = {};
    takeSettings(document('spine'), person);
    ownSettingsFrame(person);
    const frame = get(settingsFrame);
    expect(frame.status, 'the page never waits for a push that already arrived').toBe('ready');
  });

  it('draws an unreadable frame as a failed read', () => {
    const person = {};
    ownSettingsFrame(person);
    takeSettings('not a document', person);
    expect(get(settingsFrame)).toMatchObject({ status: 'failed' });
  });
});
