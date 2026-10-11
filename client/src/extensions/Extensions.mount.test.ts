// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it } from 'vitest';

import type { ExtensionsRead, RunRow, RunRowStatus } from '../wire/extensions';
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
    expect(document.body.textContent).toContain('2 plugins have updates');
  });

  it("dispatches an update for the row's own plugin and scope", () => {
    draw(extensionsWire());
    // Two stale rows: superpowers (no registry entry, so the default user
    // scope) and pensive (a local-scope entry, whose Update must carry that
    // scope - a hardcoded `user` could never move it).
    const chips = [...document.querySelectorAll('button')].filter(
      (button) => button.textContent?.trim() === 'update',
    );
    expect(chips).toHaveLength(2);
    (chips[0] as HTMLElement).click();
    flushSync();
    expect(held.commands).toEqual([
      { plugin_update: { plugin_id: 'superpowers@claude-plugins-official', scope: 'user' } },
    ]);
    (chips[1] as HTMLElement).click();
    flushSync();
    expect(held.commands[1]).toEqual({
      plugin_update: { plugin_id: 'pensive@claude-night-market', scope: 'local' },
    });
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

describe('the states the page draws', () => {
  it('draws the three marks with their words in the legend', () => {
    draw(extensionsWire());
    const legend = document.querySelector('.ext-legend');
    expect(legend).not.toBeNull();
    expect(legend?.textContent).toContain('an update is available');
    expect(legend?.textContent).toContain('work in flight');
    expect(legend?.textContent).toContain('failed - the reason is on the row');
    expect(legend?.querySelector('.dot.warn'), 'the triangle').not.toBeNull();
    expect(legend?.querySelector('.ring'), 'the spinner').not.toBeNull();
    expect(legend?.querySelector('.dot.failed'), 'the X').not.toBeNull();

    // The stale row carries the same triangle class the legend names.
    expect(document.querySelector('.ext-group .ext-row .dot.warn')).not.toBeNull();
  });

  it('draws a run in flight with its progress, and its report when finished', () => {
    const row = (id: string, status: RunRowStatus): RunRow => ({
      plugin_id: id,
      scope: 'user',
      status,
      installed_version: '6.4.1',
      available_version: '6.5.0',
      detail: null,
    });
    draw(
      extensionsWire({
        update_run: {
          trigger: 'manual',
          finished: false,
          rows: [
            row('superpowers@claude-plugins-official', 'updated'),
            row('pensive@claude-night-market', 'updating'),
          ],
        },
      }),
    );
    expect(document.body.textContent).toContain('updating 2 plugins');
    expect(document.body.textContent).toContain('1 of 2 done');

    draw(
      extensionsWire({
        update_run: {
          trigger: 'manual',
          finished: true,
          rows: [row('superpowers@claude-plugins-official', 'updated')],
        },
      }),
    );
    expect(document.body.textContent).toContain('1 plugin updated');
  });

  it('draws a failed update with its reason on the row and the restart contract', () => {
    draw(
      extensionsWire({
        update_run: {
          trigger: 'manual',
          finished: true,
          rows: [
            {
              plugin_id: 'superpowers@claude-plugins-official',
              scope: 'user',
              status: 'failed',
              installed_version: '6.4.1',
              available_version: '6.5.0',
              detail: 'Plugin "superpowers" not found',
            },
          ],
        },
      }),
    );
    expect(document.body.textContent).toContain('Plugin "superpowers" not found');

    draw(
      extensionsWire({
        update_run: {
          trigger: 'manual',
          finished: true,
          rows: [
            {
              plugin_id: 'superpowers@claude-plugins-official',
              scope: 'user',
              status: 'updated',
              installed_version: '6.5.0',
              available_version: '6.5.0',
              detail: 'restart required to apply',
            },
          ],
        },
      }),
    );
    expect(document.body.textContent).toContain('restart required');
  });

  it('draws an inventory that cannot be read as its own state alone', () => {
    draw(extensionsWire({ snapshot: null }));
    expect(document.body.textContent).toContain('could not read the plugin inventory');
    expect(document.querySelector('.ext-split'), 'no panes stand under the error').toBeNull();
  });

  it('sends an install for the chosen project when project scope is picked', () => {
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
    const dialog = document.querySelector('[role="dialog"]');
    const radios = [...(dialog?.querySelectorAll('input[type="radio"]') ?? [])];
    const projectScope = radios.find((radio) => (radio as HTMLInputElement).value === 'project');
    if (!(projectScope instanceof HTMLInputElement)) throw new Error('no project scope radio');
    projectScope.click();
    flushSync();
    const confirm = [...(dialog?.querySelectorAll('button') ?? [])].find(
      (button) => button.textContent?.trim() === 'install',
    );
    if (!(confirm instanceof HTMLButtonElement)) throw new Error('no confirm in the dialog');
    confirm.click();
    flushSync();
    expect(held.commands).toEqual([
      { plugin_install: { plugin_id: 'new@probe', scope: 'project', project: 'proj' } },
    ]);
  });
});
