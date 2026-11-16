// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it } from 'vitest';

import type { Connection } from '../socket';
import type { SettingsDocument, SettingsKey } from '../wire/settings';
import Settings from './Settings.svelte';
import { settingsFrame } from './frame';
import { emptyHome, settingsWire } from '../dev/settings.fixture';

/** The route component's own states: waiting for the frame, a frame this
 *  client could not read, and the document. */
let app: Record<string, unknown> | null = null;

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  settingsFrame.set({ status: 'waiting' });
  document.body.innerHTML = '';
});

const connection = {
  onMessage: () => () => {},
  settingsEdit: () => true,
} as unknown as Connection;

/** One engine row, as the server sends it under the projects area. */
function engineRow(path: string, group: string[], value: SettingsKey['value']): SettingsKey {
  return {
    path,
    leaf: 'engine',
    group,
    pattern: path,
    class: 'next_session',
    classLabel: 'next session',
    value,
    edit: { kind: 'enum', allowed: ['default', 'forge'], required: false, group: null },
  };
}

/** A document holding only the two engine levels, which is what the
 *  controls test reads; the fixture carries no projects area at all. */
function engineWire(project: SettingsKey['value']): SettingsDocument {
  return {
    version: 'v',
    source: '/tmp/forge.toml',
    fingerprint: 'f',
    totals: { keys: 2, areas: 1 },
    areas: [
      {
        id: 'projects',
        name: 'projects',
        note: 'orgs, their projects, and every per-project setting',
        keys: [
          engineRow('orgs.Acme.engine', ['Acme'], { kind: 'string', text: 'forge' }),
          engineRow('orgs.Acme.projects.api.engine', ['Acme', 'api'], project),
        ],
      },
    ],
  };
}

function draw(): void {
  app = mount(Settings, {
    target: document.body,
    props: { connection, home: emptyHome, mark: null },
  });
  flushSync();
}

describe('the settings route', () => {
  it('draws waiting until a frame lands', () => {
    draw();
    expect(document.body.textContent).toContain('Reading the effective config');
    expect(document.querySelector('.settings-rail'), 'no sub-nav before a document').toBeNull();
  });

  it('draws a frame this client could not read as a failed read', () => {
    settingsFrame.set({
      status: 'failed',
      detail: 'This forge sent a settings frame this client could not read.',
    });
    draw();
    expect(document.body.textContent).toContain('could not read');
  });

  it('draws the document once a frame lands', () => {
    settingsFrame.set({ status: 'ready', document: settingsWire });
    draw();
    expect(document.querySelector('.settings-rail'), 'the sub-nav drew').not.toBeNull();
    expect(document.body.textContent, 'the header names the source file').toContain(
      '~/.claude/forge/forge.toml',
    );
    expect(document.body.textContent, 'and the key total').toContain(
      String(settingsWire.totals.keys),
    );
  });

  it('draws the edit door in the ready state and toggles editing', () => {
    settingsFrame.set({ status: 'ready', document: settingsWire });
    draw();
    const door = document.querySelector<HTMLButtonElement>('.settings-mode');
    expect(door, 'the edit door is drawn').not.toBeNull();
    expect(door?.getAttribute('aria-pressed')).toBe('false');
    door?.click();
    flushSync();
    expect(door?.getAttribute('aria-pressed'), 'the one door toggles edit mode').toBe('true');
    expect(document.querySelector('.settings-card'), 'the cards stay drawn').not.toBeNull();
  });

  it('draws both engine levels and what an unset project inherits', () => {
    settingsFrame.set({ status: 'ready', document: engineWire({ kind: 'unset' }) });
    draw();
    expect(document.body.textContent, 'the org row draws its own value').toContain('forge');
    expect(
      document.body.textContent,
      'the project row says what it inherits while it sets nothing',
    ).toContain('not set · inherits forge');

    document.querySelector<HTMLButtonElement>('.settings-mode')?.click();
    flushSync();
    const selects = [...document.querySelectorAll<HTMLSelectElement>('.settings-setting select')];
    expect(selects.length, 'one control per engine level').toBe(2);
    for (const select of selects) {
      expect(
        [...select.options].map((option) => option.textContent ?? ''),
        'the choices are the accepted set, with unset offered',
      ).toEqual(['not set', 'default', 'forge']);
    }
  });
});
