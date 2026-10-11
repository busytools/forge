// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it } from 'vitest';

import type { ExtensionsRead } from '../wire/extensions';
import { components, extensionsWire, project, recorder, type Recorder } from './testing';
import ExtensionsBody from './ExtensionsBody.svelte';

/**
 * The page's controls: the rail switches panes, a plugin row folds its
 * components (one open at a time), the search filters, and the install
 * press opens the scope picker.
 *
 * The page is a view - it builds the commands and hands them out - so what
 * this pins is the command each control produces, not what the core does
 * with it.
 */
let app: Record<string, unknown> | null = null;
let held: Recorder = recorder();

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  document.body.innerHTML = '';
});

function draw(read: ExtensionsRead): void {
  held = recorder();
  app = mount(ExtensionsBody, {
    target: document.body,
    props: { wire: read, refusal: null, projects: [project], mark: null, ...held.actions },
  });
  flushSync();
}

function railItem(label: string): HTMLElement {
  const found = [...document.querySelectorAll('.ext-rail-item')].find((item) =>
    item.textContent?.includes(label),
  );
  if (!(found instanceof HTMLElement)) throw new Error(`no ${label} rail item`);
  return found;
}

function chip(text: string): HTMLButtonElement {
  const found = [...document.querySelectorAll('button')].find(
    (button) => button.textContent?.trim() === text,
  );
  if (!(found instanceof HTMLButtonElement)) throw new Error(`no button reads "${text}"`);
  return found;
}

describe('the extensions page', () => {
  it('folds one plugin at a time, and the whole row is the toggle', () => {
    draw(extensionsWire());
    const heads = [...document.querySelectorAll('.ext-group .ext-row .pick')];
    expect(heads).toHaveLength(2);

    (heads[0] as HTMLElement).click();
    flushSync();
    expect(document.body.textContent).toContain('brainstorming');
    expect(document.body.textContent).toContain('SessionStart');

    (heads[1] as HTMLElement).click();
    flushSync();
    expect(document.body.textContent).toContain('architecture-review');
    expect(
      document.body.textContent,
      'the first plugin closed when the second opened',
    ).not.toContain('brainstorming');
  });

  it('switches panes from the rail, with counts on the items', () => {
    draw(extensionsWire());
    expect(railItem('Installed').textContent).toContain('2');

    railItem('Skills').click();
    flushSync();

    expect(document.body.textContent).toContain('brainstorming');
    expect(document.body.textContent).toContain('superpowers');
    expect(
      document.querySelectorAll('.ext-group'),
      'the plugin rows are not the pane anymore',
    ).toHaveLength(0);
    expect(
      document.querySelectorAll('.ext-row.flat').length,
      'the kind pane draws its own flat rows',
    ).toBeGreaterThan(1);
  });

  it('filters the pane as the search is typed', () => {
    draw(extensionsWire());
    const input = document.querySelector<HTMLInputElement>('.find');
    if (input === null) throw new Error('no search field');
    input.value = 'pensive';
    input.dispatchEvent(new Event('input', { bubbles: true }));
    flushSync();

    const rows = [...document.querySelectorAll('.ext-group')].map((row) => row.textContent ?? '');
    expect(rows).toHaveLength(1);
    expect(rows[0]).toContain('pensive');
    // The band is the page's update state, not the query's: it still names
    // what is stale even when the row itself is filtered out.
    expect(document.body.textContent).toContain('1 plugin has an update');
  });

  it("dispatches an update for the row's own plugin and scope", () => {
    draw(extensionsWire());
    chip('update').click();
    flushSync();
    expect(held.commands).toEqual([
      { plugin_update: { plugin_id: 'superpowers@claude-plugins-official', scope: 'user' } },
    ]);
  });

  it('dispatches update all from the band', () => {
    draw(extensionsWire());
    chip('update all').click();
    flushSync();
    expect(held.commands).toEqual(['plugin_update_all']);
  });

  it('opens the scope picker on install and sends the chosen scope', () => {
    draw(
      extensionsWire({
        snapshot: {
          installed: [],
          marketplace: [],
          marketplaces: [],
          components: [
            components({ plugin: 'new@probe', installed: false, version: null, skills: ['s'] }),
          ],
          marketplace_health: [],
        },
      }),
    );
    chip('install').click();
    flushSync();
    const dialog = document.querySelector('[role="dialog"]');
    expect(dialog).not.toBeNull();

    // The dialog's own confirm sends the install for the default scope.
    const confirm = [...(dialog?.querySelectorAll('button') ?? [])].find(
      (button) => button.textContent?.trim() === 'install',
    );
    if (!(confirm instanceof HTMLButtonElement)) throw new Error('no confirm in the dialog');
    confirm.click();
    flushSync();
    expect(held.commands).toEqual([
      { plugin_install: { plugin_id: 'new@probe', scope: 'user', project: null } },
    ]);
    expect(document.querySelector('[role="dialog"]'), 'the dialog closed on confirm').toBeNull();
  });

  it('closes the scope picker on cancel', () => {
    draw(
      extensionsWire({
        snapshot: {
          installed: [],
          marketplace: [],
          marketplaces: [],
          components: [components({ plugin: 'new@probe', installed: false })],
          marketplace_health: [],
        },
      }),
    );
    chip('install').click();
    flushSync();
    chip('cancel').click();
    flushSync();
    expect(document.querySelector('[role="dialog"]')).toBeNull();
    expect(held.commands).toEqual([]);
  });

  it('repairs a drifted marketplace by name', () => {
    draw(extensionsWire());
    railItem('Marketplaces').click();
    flushSync();
    chip('repair').click();
    flushSync();
    expect(held.commands).toEqual([{ marketplace_repair: { name: 'stx-clarity' } }]);
  });
});
