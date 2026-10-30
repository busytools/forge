import { get } from 'svelte/store';
import { describe, expect, it } from 'vitest';

import { applySettings } from '../theme';
import { DEFAULT_SETTINGS } from '../wire/types';
import { clientAxes, ownSettingsFrame, settingsFrame, takeSettings } from './frame';

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

  /**
   * **An unset axis is no setting at all.** The document carries `null`; the
   * axes path must yield `font: null`, and the built-in stack is what
   * applies. A display default fed back here would reach `fontStack` as an
   * unknown name and remove `--ui` (Times).
   */
  it('applies the built-in stack for an unset font', () => {
    const owner = {};
    ownSettingsFrame(owner);
    takeSettings(
      {
        version: 'v',
        source: '~/.claude/forge/forge.toml',
        totals: { keys: 3, areas: 1 },
        areas: [
          {
            id: 'client',
            name: 'client',
            note: '',
            keys: [
              { path: 'client.mark', kind: 'string', value: null, class: 'live' },
              { path: 'client.theme', kind: 'string', value: null, class: 'live' },
              { path: 'client.font', kind: 'string', value: null, class: 'live' },
            ],
          },
        ],
      },
      owner,
    );
    const axes = clientAxes(get(settingsFrame));
    expect(axes, 'an unset axis yields no name').toEqual({ mark: null, theme: null, font: null });

    const applied: Record<string, string> = {};
    applySettings(
      { ...DEFAULT_SETTINGS, ...axes },
      {
        style: {
          setProperty: (token: string, value: string) => {
            applied[token] = value;
          },
          removeProperty: (token: string) => {
            delete applied[token];
          },
        },
      },
    );
    expect(applied['--ui'], 'the built-in stack applies, so the page is not in Times').toContain(
      'Fira Code',
    );
  });
});
