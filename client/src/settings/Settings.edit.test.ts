// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import type { ServerMessage } from '../protocol';
import type { Connection } from '../socket';
import type { SettingsDocument, SettingsEditWire } from '../wire/settings';
import Settings from './Settings.svelte';
import { settingsFrame } from './frame';
import { emptyHome, settingsWire } from '../dev/settings.fixture';

/**
 * The editor's page: entering edit mode, a batched save's wire, the push that
 * answers it, and the refusal the bar draws.
 */
let app: Record<string, unknown> | null = null;
let listeners: Set<(message: ServerMessage) => void>;
let sent: { fingerprint: string; edits: SettingsEditWire[] }[];

function connection(): Connection {
  return {
    onMessage: (fn: (message: ServerMessage) => void) => {
      listeners.add(fn);
      return () => listeners.delete(fn);
    },
    settingsEdit: (fingerprint: string, edits: SettingsEditWire[]) => {
      sent.push({ fingerprint, edits });
      return true;
    },
  } as unknown as Connection;
}

function draw(settingsDocument: SettingsDocument = settingsWire): void {
  settingsFrame.set({ status: 'ready', document: settingsDocument });
  app = mount(Settings, {
    target: document.body,
    props: { connection: connection(), home: emptyHome, mark: null },
  });
  flushSync();
}

function rail(label: string): HTMLElement {
  const found = [...document.querySelectorAll<HTMLElement>('.settings-rail-item')].find((item) =>
    item.textContent?.includes(label),
  );
  if (found === undefined) throw new Error(`no ${label} area`);
  return found;
}

beforeEach(() => {
  listeners = new Set();
  sent = [];
});

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  settingsFrame.set({ status: 'waiting' });
  document.body.innerHTML = '';
});

describe('the settings editor page', () => {
  it('edits a value, sends one batched save, and draws the push it answers', () => {
    draw();
    (document.querySelector('.settings-mode') as HTMLButtonElement).click();
    flushSync();
    rail('server').click();
    flushSync();

    const port =
      [...document.querySelectorAll<HTMLElement>('.settings-rows tr')]
        .find((row) => row.querySelector('code')?.getAttribute('title') === 'server.port')
        ?.querySelector<HTMLInputElement>('.number-field') ?? null;
    if (port === null) throw new Error('the number control is not drawn');
    port.value = '8791';
    port.dispatchEvent(new Event('input', { bubbles: true }));
    flushSync();
    expect(document.body.textContent, 'the bar counts the change').toContain('1 unsaved');

    (document.querySelector('.settings-btn.go') as HTMLButtonElement).click();
    flushSync();
    expect(sent, 'one save went').toHaveLength(1);
    expect(sent[0]?.fingerprint).toBe(settingsWire.fingerprint);
    expect(sent[0]?.edits).toEqual([
      {
        area: 'server',
        key_path: 'server.port',
        pattern: 'server.port',
        op: 'set',
        value: 8791,
      },
    ]);

    // The reload's push is the save's answer.
    const after: SettingsDocument = {
      ...settingsWire,
      fingerprint: 'ffffffffffffffff',
      areas: settingsWire.areas.map((area) =>
        area.id === 'server'
          ? {
              ...area,
              keys: area.keys.map((held) =>
                held.path === 'server.port'
                  ? { ...held, value: { kind: 'number' as const, text: '8791' } }
                  : held,
              ),
            }
          : area,
      ),
    };
    settingsFrame.set({ status: 'ready', document: after });
    flushSync();

    expect(document.querySelector('.settings-savebar'), 'the bar cleared').toBeNull();
    expect(
      document.querySelector('.settings-restart')?.textContent,
      'the restart banner names it',
    ).toContain('server.port');
  });

  it('draws the refusal a save that wrote nothing returns', () => {
    draw();
    (document.querySelector('.settings-mode') as HTMLButtonElement).click();
    flushSync();
    rail('server').click();
    flushSync();
    const port =
      [...document.querySelectorAll<HTMLElement>('.settings-rows tr')]
        .find((row) => row.querySelector('code')?.getAttribute('title') === 'server.port')
        ?.querySelector<HTMLInputElement>('.number-field') ?? null;
    if (port === null) throw new Error('the number control is not drawn');
    port.value = '8791';
    port.dispatchEvent(new Event('input', { bubbles: true }));
    flushSync();
    (document.querySelector('.settings-btn.go') as HTMLButtonElement).click();
    flushSync();

    for (const listener of listeners) {
      listener({
        kind: 'settings_refused',
        reason: 'invalid',
        message: 'forge.toml at /tmp/forge.toml failed to parse in `port`: not a number',
        key_path: 'server.port',
      });
    }
    flushSync();
    expect(document.body.textContent, 'the loader own words are on the bar').toContain(
      'not a number',
    );
    expect(
      document.querySelector('.settings-rows tr.bad code')?.getAttribute('title'),
      'and the row the loader named is marked',
    ).toBe('server.port');
  });

  it('adds a skeleton entry and removes one behind a confirm', () => {
    draw();
    (document.querySelector('.settings-mode') as HTMLButtonElement).click();
    flushSync();

    const add = [...document.querySelectorAll<HTMLButtonElement>('.settings-add')].find((button) =>
      button.textContent?.includes('account'),
    );
    if (add === undefined) throw new Error('the area header carries no add door');
    add.click();
    flushSync();
    expect(document.body.textContent, 'the skeleton is drawn').toContain('new-account');
    expect(document.body.textContent, 'and its required fields are marked').toContain('required');

    // The skeleton's add is part of the same batched save.
    (document.querySelector('.settings-btn.go') as HTMLButtonElement).click();
    flushSync();
    expect(sent[0]?.edits.some((edit) => edit.op === 'add')).toBe(true);

    // A removal asks first, naming the entry.
    const granite = [...document.querySelectorAll<HTMLElement>('.settings-group')].find((group) =>
      group.textContent?.includes('Granite'),
    );
    const remove = granite?.querySelector<HTMLButtonElement>('.settings-row-action.danger');
    if (remove === undefined || remove === null)
      throw new Error('the entry carries no remove door');
    remove.click();
    flushSync();
    expect(
      document.querySelector('.settings-dialog')?.textContent,
      'the confirm names it',
    ).toContain('Granite');
    const confirm = [
      ...document.querySelectorAll<HTMLButtonElement>('.settings-dialog .settings-btn'),
    ].find((button) => button.textContent === 'remove');
    if (confirm === undefined) throw new Error('the confirm has no remove button');
    confirm.click();
    flushSync();
    expect(document.body.textContent, 'the group reads as removed').toContain('removed on save');
  });
});
