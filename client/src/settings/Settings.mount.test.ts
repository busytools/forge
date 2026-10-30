// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it } from 'vitest';

import SettingsBody from './SettingsBody.svelte';
import { settingsWire } from './testing';

/**
 * The page's controls: the sub-nav switches the pane, and an area that is
 * gone or empty draws as itself. The page is a view; what it dispatches is
 * nothing, so what this pins is the drawing and the id the rail hands up.
 */
let app: Record<string, unknown> | null = null;

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  document.body.innerHTML = '';
});

function draw(selected: string | null, onSelect: (id: string) => void = () => {}): void {
  app = mount(SettingsBody, {
    target: document.body,
    props: { document: settingsWire, mark: null, cliVersion: 'v2.1.280', selected, onSelect },
  });
  flushSync();
}

function rail(): HTMLElement[] {
  return [...document.querySelectorAll<HTMLElement>('.settings-rail-item')];
}

describe('the settings page', () => {
  it('draws every area in the sub-nav with its key count', () => {
    draw(null);
    expect(rail()).toHaveLength(settingsWire.areas.length);
    expect(rail()[0]?.textContent, 'the first area is named and counted').toContain('accounts');
    expect(rail()[0]?.textContent).toContain('6');
    expect(rail()[3]?.textContent, 'an empty area still has a row').toContain('models');
    expect(rail()[3]?.textContent).toContain('0');
  });

  it('draws the selected area rows with their class words', () => {
    draw('server');
    expect(document.querySelector('#settings-area-title')?.textContent).toBe('server');
    const keys = [...document.querySelectorAll('.settings-rows th[scope="row"]')].map(
      (cell) => cell.textContent,
    );
    expect(keys).toEqual(['server.port', 'server.first_frame_cap_kb']);
    expect(document.body.textContent, 'the port is drawn as a restart').toContain('restart');
    expect(document.body.textContent, 'and its value').toContain('8790');
  });

  it('groups the accounts by account', () => {
    draw('accounts');
    const groups = [...document.querySelectorAll('.settings-group-name')].map(
      (name) => name.textContent,
    );
    expect(groups, 'a key-major table still reads one account at a time').toEqual([
      'Granite',
      'Subspace',
    ]);
    expect(
      [...document.querySelectorAll('.settings-rows th[scope="row"]')].map(
        (cell) => cell.textContent,
      ),
      'each account keeps its own rows',
    ).toContain('accounts.Subspace.token');
  });

  it('hands the picked area id up', () => {
    const picked: string[] = [];
    draw(null, (id) => picked.push(id));
    const server = rail().find((item) => item.textContent?.includes('server'));
    expect(server, 'the rail has the server area').toBeDefined();
    server?.click();
    expect(picked).toEqual(['server']);
  });

  it('draws an area with no keys as itself', () => {
    draw('models');
    expect(document.body.textContent).toContain('This area has no keys');
  });

  it('draws a selected area the document no longer holds, with a way back', () => {
    draw('dictate');
    expect(document.body.textContent).toContain('That area is not in this config');
    expect(document.querySelector('.settings-link')?.textContent).toContain('Open accounts');
  });

  it('draws a class this client does not know with its own word', () => {
    draw('connectors');
    expect(document.body.textContent, 'the unread key is drawn as not read yet').toContain(
      'not read yet',
    );
    expect(document.body.textContent, 'and an unknown class keeps the server word').toContain(
      'some future class',
    );
  });
});
