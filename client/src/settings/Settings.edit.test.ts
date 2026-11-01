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
 * answers it, and the one claim the bar draws.
 */
let app: Record<string, unknown> | null = null;
let listeners: Set<(message: ServerMessage) => void>;
let sent: { fingerprint: string; edits: SettingsEditWire[]; id: number }[];
/** Whether the connection takes the next save. */
let accepts: boolean;

function connection(): Connection {
  return {
    onMessage: (fn: (message: ServerMessage) => void) => {
      listeners.add(fn);
      return () => listeners.delete(fn);
    },
    settingsEdit: (fingerprint: string, edits: SettingsEditWire[], id: number) => {
      sent.push({ fingerprint, edits, id });
      return accepts;
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

/** The line for one key, found by the path its code carries. */
function setting(path: string): HTMLElement {
  const found = [...document.querySelectorAll<HTMLElement>('.settings-setting')].find(
    (row) => row.querySelector('code')?.getAttribute('title') === path,
  );
  if (found === undefined) throw new Error(`no ${path} setting`);
  return found;
}

/** The card one instance draws, found by its heading. */
function card(name: string): HTMLElement {
  const found = [...document.querySelectorAll<HTMLElement>('.settings-card')].find((held) =>
    (
      held.querySelector('.settings-card-head h3, .settings-card-head .settings-card-name')
        ?.textContent ?? ''
    ).includes(name),
  );
  if (found === undefined) throw new Error(`no ${name} card`);
  return found;
}

/** Enter edit mode and open the server area. */
function editing(): void {
  (document.querySelector('.settings-mode') as HTMLButtonElement).click();
  flushSync();
  rail('server').click();
  flushSync();
}

/** Change the port through its control. */
function changePort(): void {
  const port = setting('server.port').querySelector<HTMLInputElement>('.number-field');
  if (port === null) throw new Error('the number control is not drawn');
  port.value = '8791';
  port.dispatchEvent(new Event('input', { bubbles: true }));
  flushSync();
}

beforeEach(() => {
  listeners = new Set();
  sent = [];
  accepts = true;
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
    editing();
    changePort();
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
    editing();
    changePort();
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
      document.querySelector('.settings-setting.bad code')?.getAttribute('title'),
      'and the line the loader named is marked',
    ).toBe('server.port');
  });

  /**
   * A pop the confirm's history entry queues needs a task to land, and under
   * a loaded runner one tick is not always enough: wait on the condition,
   * bounded, rather than on a count of ticks.
   */
  async function settle(until: () => boolean = () => true): Promise<void> {
    for (let at = 0; at < 20; at += 1) {
      await new Promise((resolve) => setTimeout(resolve, 0));
      flushSync();
      if (until()) return;
    }
  }

  it('draws a foreign push as a conflict instead of rebasing the draft', () => {
    draw();
    editing();
    changePort();

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

  /**
   * **The bar makes one claim.** A landed save keeps its flash for a moment;
   * an edit, then a foreign push, must replace it rather than draw beside it:
   * "1 unsaved saved forge.toml changed" states two things at once.
   */
  it('clears the saved flash when a foreign push opens a conflict', () => {
    draw();
    editing();
    changePort();
    (document.querySelector('.settings-btn.go') as HTMLButtonElement).click();
    flushSync();
    hear({ kind: 'settings_saved', id: sent[0]?.id ?? 0 });
    const bar = document.querySelector('.settings-savebar');
    expect(bar?.querySelector('.saved'), 'the save flashed saved').not.toBeNull();

    changePort();
    settingsFrame.set({
      status: 'ready',
      document: { ...settingsWire, fingerprint: 'ffffffffffffffff' },
    });
    flushSync();

    const after = document.querySelector('.settings-savebar');
    expect(after?.textContent, 'the conflict is the bar claim').toContain(
      'changed since this page loaded it',
    );
    expect(after?.querySelector('.saved'), 'and the saved flash is gone').toBeNull();
  });

  /** A refusal outranks the flash in the one slot: only the refusal draws. */
  it('draws only the refusal when it lands after a saved flash', () => {
    draw();
    editing();
    changePort();
    (document.querySelector('.settings-btn.go') as HTMLButtonElement).click();
    flushSync();
    hear({ kind: 'settings_saved', id: sent[0]?.id ?? 0 });
    expect(document.querySelector('.settings-savebar .saved')).not.toBeNull();

    changePort();
    accepts = false;
    (document.querySelector('.settings-btn.go') as HTMLButtonElement).click();
    flushSync();

    const bar = document.querySelector('.settings-savebar');
    expect(bar?.textContent, 'the refusal is the bar claim').toContain('nothing was sent');
    expect(bar?.querySelector('.saved'), 'and the saved flash is gone').toBeNull();
  });

  /**
   * **A refusal must not close the conflict's door.** A save refused for
   * anything but a stale file leaves the draft dirty; a foreign push then
   * sets the conflict, and with the refusal still in the one slot the reader
   * needs the conflict's reload, not only Discard.
   */
  it('keeps the reload door when a conflict lands behind a refusal', () => {
    draw();
    editing();
    changePort();
    (document.querySelector('.settings-btn.go') as HTMLButtonElement).click();
    flushSync();
    hear({
      kind: 'settings_refused',
      reason: 'invalid',
      message: 'forge.toml at /tmp/forge.toml failed to parse in `port`: not a number',
      id: sent[0]?.id ?? 0,
      key_path: 'server.port',
    });
    settingsFrame.set({
      status: 'ready',
      document: { ...settingsWire, fingerprint: 'ffffffffffffffff' },
    });
    flushSync();

    const bar = document.querySelector('.settings-savebar');
    expect(bar?.textContent, 'the refusal stays the claim').toContain('not a number');
    expect(
      [...(bar?.querySelectorAll<HTMLButtonElement>('.settings-btn') ?? [])].some(
        (button) => button.textContent === 'reload',
      ),
      'the conflict reload is still offered',
    ).toBe(true);
    expect(
      (document.querySelector('.settings-btn.go') as HTMLButtonElement).disabled,
      'saving is still withheld',
    ).toBe(true);
  });

  it('asks before leaving a dirty draft', async () => {
    draw();
    editing();
    changePort();

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
    await settle(() => document.querySelector('.settings-savebar') === null);
    expect(document.querySelector('.settings-mode')?.textContent, 'editing ended').toContain(
      'edit',
    );
    expect(document.querySelector('.settings-savebar'), 'the draft is gone').toBeNull();
  });

  it('ignores an acknowledgement that is not this save', () => {
    draw();
    editing();
    changePort();
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
    const remove = card('Granite').querySelector<HTMLButtonElement>('.settings-row-action.danger');
    if (remove === null) throw new Error('the card carries no remove door');
    remove.click();
    flushSync();
    expect(document.querySelector('.settings-dialog')?.textContent).toContain('Granite');

    [...document.querySelectorAll<HTMLButtonElement>('.settings-dialog .settings-btn')]
      .find((button) => button.textContent === 'cancel')
      ?.click();
    await settle(() => document.querySelector('.settings-dialog') === null);
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
    editing();
    changePort();

    goTo({ name: 'home' });
    flushSync();
    expect(get(currentRoute).name, 'the page did not move').toBe('settings');
    expect(document.querySelector('.settings-dialog')?.textContent).toContain(
      'Discard these changes?',
    );
  });

  /**
   * **A save's answer can land while the discard confirm is open.** The
   * draft empties under the dialog, and confirming must still leave - the
   * leave rides the draft entry's own pop, which the page must notice even
   * though `dirty` stopped changing.
   */
  it('a save answer landing under the discard confirm still moves the page', async () => {
    currentRoute.set({ name: 'settings' });
    draw();
    editing();
    changePort();
    (document.querySelector('.settings-btn.go') as HTMLButtonElement).click();
    flushSync();

    goTo({ name: 'home' });
    flushSync();
    expect(document.querySelector('.settings-dialog')?.textContent).toContain(
      'Discard these changes?',
    );

    // The save's own acknowledgement empties the draft under the dialog.
    hear({ kind: 'settings_saved', id: sent[0]?.id ?? 0 });
    expect(document.body.textContent, 'the ack cleared the draft').not.toContain('1 unsaved');
    [...document.querySelectorAll<HTMLButtonElement>('.settings-dialog .settings-btn')]
      .find((button) => button.textContent === 'discard')
      ?.click();
    await settle(() => get(currentRoute).name === 'home');
    expect(get(currentRoute).name, 'the held move ran on the confirm').toBe('home');
  });

  /**
   * **Cancelling under a save's answer does not leave the draft entry on the
   * stack.** The draft emptied while the dialog was open, so nothing else
   * drops its entry; a plain `let` for the entry flags left it behind and the
   * next Back asked about a draft that was gone.
   */
  it('a save answer landing under the discard confirm leaves no draft entry on cancel', async () => {
    currentRoute.set({ name: 'settings' });
    // A sentinel under the page's own entries, so the state read at the end
    // is this test's and not a neighbour's.
    history.pushState({ clean: true }, '', location.href);
    draw();
    editing();
    changePort();
    (document.querySelector('.settings-btn.go') as HTMLButtonElement).click();
    flushSync();

    goTo({ name: 'home' });
    flushSync();
    hear({ kind: 'settings_saved', id: sent[0]?.id ?? 0 });
    expect(document.body.textContent, 'the ack cleared the draft').not.toContain('1 unsaved');
    [...document.querySelectorAll<HTMLButtonElement>('.settings-dialog .settings-btn')]
      .find((button) => button.textContent === 'cancel')
      ?.click();
    // The draft entry carries the sentinel's own fields onward, so wait on
    // the mark being gone, not on the sentinel being current.
    await settle(
      () => (history.state as Record<string, unknown> | null)?.forgeSettingsDraft === undefined,
    );
    expect(
      (history.state as Record<string, unknown> | null)?.forgeSettingsDraft,
      'the draft entry was dropped on the cancel',
    ).toBeUndefined();
    expect(
      (history.state as Record<string, unknown> | null)?.clean,
      'and the page is back on its own entry',
    ).toBe(true);
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
    expect(document.body.textContent, 'and the card says what it is').toContain('new account');

    // The skeleton's add is part of the same batched save.
    (document.querySelector('.settings-btn.go') as HTMLButtonElement).click();
    flushSync();
    expect(sent[0]?.edits.some((edit) => edit.op === 'add')).toBe(true);

    // A removal asks first, naming the entry.
    const remove = card('Granite').querySelector<HTMLButtonElement>('.settings-row-action.danger');
    if (remove === null) throw new Error('the card carries no remove door');
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
    // The answer rides the dialog entry's own pop, so the removal lands after
    // the click.
    await settle(() => (document.body.textContent ?? '').includes('removed on save'));
    expect(document.querySelector('.settings-dialog'), 'the dialog is gone').toBeNull();
    expect(document.body.textContent, 'the card reads as removed').toContain('removed on save');
  });
});
