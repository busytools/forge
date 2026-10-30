// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import type { ServerMessage } from '../protocol';
import { currentRoute, goTo } from '../routes';
import { get } from 'svelte/store';
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
let sent: { fingerprint: string; edits: SettingsEditWire[]; id: number }[];

function connection(): Connection {
  return {
    onMessage: (fn: (message: ServerMessage) => void) => {
      listeners.add(fn);
      return () => listeners.delete(fn);
    },
    settingsEdit: (fingerprint: string, edits: SettingsEditWire[], id: number) => {
      sent.push({ fingerprint, edits, id });
      return true;
    },
  } as unknown as Connection;
}

/** Deliver one server frame to every listener the page installed. */
function hear(message: ServerMessage): void {
  for (const listener of listeners) listener(message);
  flushSync();
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

    // The server's own acknowledgement, matched by the save's id, clears the
    // draft; the reload's push follows.
    hear({ kind: 'settings_saved', id: sent[0]?.id ?? 0 });

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

    expect(document.body.textContent, 'the bar says the save landed').toContain('saved');
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

    hear({
      kind: 'settings_refused',
      reason: 'invalid',
      message: 'forge.toml at /tmp/forge.toml failed to parse in `port`: not a number',
      id: sent[0]?.id ?? 0,
      key_path: 'server.port',
    });
    expect(document.body.textContent, 'the loader own words are on the bar').toContain(
      'not a number',
    );
    expect(
      document.querySelector('.settings-rows tr.bad code')?.getAttribute('title'),
      'and the row the loader named is marked',
    ).toBe('server.port');
  });

  /** A pop the confirm's history entry queues needs a task to land. */
  async function settle(): Promise<void> {
    await new Promise((resolve) => setTimeout(resolve, 0));
    flushSync();
  }

  it('draws a foreign push as a conflict instead of rebasing the draft', () => {
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

    settingsFrame.set({
      status: 'ready',
      document: { ...settingsWire, fingerprint: 'ffffffffffffffff' },
    });
    flushSync();

    expect(document.body.textContent, 'the conflict is drawn').toContain(
      'changed since this page loaded it',
    );
    expect(document.body.textContent, 'the draft is kept').toContain('1 unsaved');
    expect(
      (document.querySelector('.settings-btn.go') as HTMLButtonElement).disabled,
      'saving against the old baseline is not offered',
    ).toBe(true);
  });

  it('asks before leaving a dirty draft', async () => {
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

    (document.querySelector('.settings-mode') as HTMLButtonElement).click();
    flushSync();
    expect(document.querySelector('.settings-dialog')?.textContent).toContain(
      'Discard these changes?',
    );
    [...document.querySelectorAll<HTMLButtonElement>('.settings-dialog .settings-btn')]
      .find((button) => button.textContent === 'cancel')
      ?.click();
    await settle();
    await settle();
    expect(document.querySelector('.settings-dialog'), 'the dialog is gone').toBeNull();
    expect(document.querySelector('.settings-mode')?.textContent, 'still editing').toContain(
      'editing',
    );
    expect(document.body.textContent, 'the draft is still there').toContain('1 unsaved');

    (document.querySelector('.settings-mode') as HTMLButtonElement).click();
    flushSync();
    [...document.querySelectorAll<HTMLButtonElement>('.settings-dialog .settings-btn')]
      .find((button) => button.textContent === 'discard')
      ?.click();
    await settle();
    expect(document.querySelector('.settings-mode')?.textContent, 'editing ended').toContain(
      'edit',
    );
    expect(document.querySelector('.settings-savebar'), 'the draft is gone').toBeNull();
  });

  it('ignores an acknowledgement that is not this save', () => {
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

    hear({ kind: 'settings_saved', id: (sent[0]?.id ?? 0) + 1 });
    expect(document.body.textContent, 'another save did not clear this draft').toContain(
      '1 unsaved',
    );
  });

  /**
   * **One owner of popstate.** The dialog's entry and the page's dirty guard
   * both answering a pop reopened the discard dialog for ever: cancelling a
   * removal raised a spurious one.
   */
  it('cancelling a removal confirm closes it without raising a discard', async () => {
    draw();
    (document.querySelector('.settings-mode') as HTMLButtonElement).click();
    flushSync();
    const granite = [...document.querySelectorAll<HTMLElement>('.settings-group')].find((group) =>
      group.textContent?.includes('Granite'),
    );
    const remove = granite?.querySelector<HTMLButtonElement>('.settings-row-action.danger');
    if (remove === undefined || remove === null)
      throw new Error('the entry carries no remove door');
    remove.click();
    flushSync();
    expect(document.querySelector('.settings-dialog')?.textContent).toContain('Granite');

    [...document.querySelectorAll<HTMLButtonElement>('.settings-dialog .settings-btn')]
      .find((button) => button.textContent === 'cancel')
      ?.click();
    await settle();
    await settle();
    expect(document.querySelector('.settings-dialog'), 'the dialog is gone').toBeNull();
    expect(document.body.textContent, 'no discard confirm was raised').not.toContain(
      'Discard these changes?',
    );
  });

  /**
   * **A programmatic move asks too.** A notification's Open is not a person
   * pressing Back, and losing the draft to it would be silent.
   */
  it('asks before a programmatic move while the draft is dirty', () => {
    currentRoute.set({ name: 'settings' });
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

    goTo({ name: 'home' });
    flushSync();
    expect(get(currentRoute).name, 'the page did not move').toBe('settings');
    expect(document.querySelector('.settings-dialog')?.textContent).toContain(
      'Discard these changes?',
    );
  });

  it('adds a skeleton entry and removes one behind a confirm', async () => {
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
    // The answer rides the dialog entry's own pop, so the removal lands one
    // task after the click.
    await settle();
    await settle();
    expect(document.querySelector('.settings-dialog'), 'the dialog is gone').toBeNull();
    expect(document.body.textContent, 'the group reads as removed').toContain('removed on save');
  });
});
