// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it } from 'vitest';

import SettingsBody from './SettingsBody.svelte';
import { settingsWire } from '../dev/settings.fixture';
import { emptyDraft } from './editor';

/**
 * The page's controls: the sub-nav switches the pane, each instance is a
 * card, each key a line inside it, and an area that is gone or empty draws as
 * itself. The page is a view; what it dispatches is nothing, so what this pins
 * is the drawing and the id the rail hands up.
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
    props: {
      document: settingsWire,
      mark: null,
      cliVersion: '2.1.280',
      selected,
      onSelect,
      draft: emptyDraft(),
      onToggleEditing: () => {},
      onValue: () => {},
      onEntryValue: () => {},
      onAdd: () => {},
      onRemoveRequest: () => {},
      onCancelAdd: () => {},
      onUndoRemove: () => {},
    },
  });
  flushSync();
}

function rail(): HTMLElement[] {
  return [...document.querySelectorAll<HTMLElement>('.settings-rail-item')];
}

function cards(): HTMLElement[] {
  return [...document.querySelectorAll<HTMLElement>('.settings-card')];
}

function cardNames(): (string | undefined)[] {
  return cards().map(
    (card) =>
      card.querySelector('.settings-card-head h3, .settings-card-head .settings-card-name')
        ?.textContent ?? undefined,
  );
}

function rows(): HTMLElement[] {
  return [...document.querySelectorAll<HTMLElement>('.settings-setting')];
}

function rowKeys(): (string | undefined)[] {
  return rows().map(
    (row) => row.querySelector('.settings-setting-label code')?.textContent?.trim() ?? undefined,
  );
}

describe('the settings page', () => {
  it('draws every area in the sub-nav with its key count', () => {
    draw(null);
    expect(rail()).toHaveLength(settingsWire.areas.length);
    expect(rail()[0]?.textContent, 'the first area is named and counted').toContain('accounts');
    expect(rail()[0]?.textContent).toContain(String(settingsWire.areas[0]?.keys.length));
    expect(rail()[4]?.textContent, 'an empty area still has a row').toContain('documents');
    expect(rail()[4]?.textContent).toContain('0');
  });

  it('draws each instance as a card and each key as one line in it', () => {
    draw('server');
    expect(document.querySelector('#settings-area-title')?.textContent).toBe('server');
    expect(cardNames(), 'each table is its own card, named and counted').toEqual([
      'browser',
      'server',
    ]);
    expect(cards()[1]?.querySelector('.settings-card-count')?.textContent).toBe('2 keys');
    expect(cards()[0]?.querySelector('.settings-card-count')?.textContent).toBe('1 key');

    expect(rowKeys(), 'a line draws its leaf, not the path its card already names').toEqual([
      'idle_minutes',
      'port',
      'first_frame_cap_kb',
    ]);
    const port = rows().find(
      (row) => row.querySelector('code')?.getAttribute('title') === 'server.port',
    );
    expect(port, 'the whole path stays reachable').toBeDefined();
    expect(
      port?.querySelector<HTMLElement>('.settings-class-word')?.textContent,
      'the line states its class once',
    ).toBe('restart');
    expect(port?.querySelector('.settings-setting-value')?.textContent?.trim()).toBe('8790');
    expect(
      rows().every((row) => row.querySelectorAll('.settings-class').length === 1),
      'no line states its class twice',
    ).toBe(true);
    expect(document.querySelector('.settings-legend'), 'the legend band is gone').toBeNull();
  });

  it('groups the accounts by account', () => {
    draw('accounts');
    expect(cardNames(), 'a card per account, in name order').toEqual([
      'Acme.Corp',
      'Granite',
      'OpenRouter',
      'Subspace',
    ]);
    expect(rowKeys(), 'each account keeps its own rows').toContain('token');
  });

  it('draws a dotted leaf and a dotted group whole', () => {
    draw('accounts');
    const slug = rows()
      .map((row) => row.querySelector<HTMLElement>('.settings-setting-label code'))
      .find(
        (code) => code?.getAttribute('title') === 'accounts.OpenRouter.model_slugs.glm-5.3-flash',
      );
    expect(slug?.textContent, 'the slug keeps its dots rather than losing all but 3-flash').toBe(
      'glm-5.3-flash',
    );
    expect(cardNames(), 'a dotted account name stays one card heading').toContain('Acme.Corp');
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
    draw('documents');
    expect(document.body.textContent).toContain('This area has no keys');
  });

  it('groups the models by model, which the card heading names', () => {
    draw('models');
    expect(
      cardNames(),
      'each model is its own card, so the model is visible rather than a tooltip',
    ).toEqual(['claude-opus-5-5', 'deepseek-v4.1-flash']);
    expect(rowKeys(), 'the line keeps the key name the card does not repeat').toEqual([
      'auto_compact_window',
      'auto_compact_window',
    ]);
  });

  it('splits the server area by the table each row comes from', () => {
    draw('server');
    expect(cardNames()).toEqual(['browser', 'server']);
  });

  it('draws a selected area the document no longer holds, with a way back', () => {
    draw('deprecated');
    expect(document.body.textContent).toContain('That area is not in this config');
    const back = document.querySelector<HTMLButtonElement>('.settings-link');
    expect(back?.textContent).toContain('Open accounts');
    back?.click();
    expect(document.activeElement?.id, 'the reader lands on the pane it opened').toBe(
      'settings-area-title',
    );
  });

  it('draws every class an area holds on its rail row', () => {
    draw(null);
    const connectors = rail().find((item) => item.textContent?.includes('connectors'));
    expect(connectors?.textContent, 'the unknown class is drawn').toContain('unknown class');
    expect(connectors?.textContent, 'and the unread key beside it').toContain('not read yet');
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

  it('draws the built-in a client axis falls back to', () => {
    const spec = { kind: 'string' as const, allowed: [], required: false, group: null };
    const settingsDocument = {
      version: 'v',
      source: '~/.claude/forge/forge.toml',
      fingerprint: '0123456789abcdef',
      totals: { keys: 2, areas: 1 },
      areas: [
        {
          id: 'client',
          name: 'client',
          note: 'the sets a client draws with',
          keys: [
            {
              path: 'client.mark',
              leaf: 'mark',
              group: [],
              pattern: 'client.mark',
              class: 'live' as const,
              classLabel: 'live',
              value: { kind: 'string' as const, text: 'klin' },
              edit: spec,
            },
            {
              path: 'client.font',
              leaf: 'font',
              group: [],
              pattern: 'client.font',
              class: 'live' as const,
              classLabel: 'live',
              value: { kind: 'unset' as const },
              edit: spec,
            },
          ],
        },
      ],
    };
    app = mount(SettingsBody, {
      target: document.body,
      props: {
        document: settingsDocument,
        mark: null,
        cliVersion: null,
        selected: 'client',
        onSelect: () => {},
        draft: emptyDraft(),
        onToggleEditing: () => {},
        onValue: () => {},
        onEntryValue: () => {},
        onAdd: () => {},
        onRemoveRequest: () => {},
        onCancelAdd: () => {},
        onUndoRemove: () => {},
      },
    });
    flushSync();
    expect(
      document.body.textContent,
      'the unset font names the bundled face, not a settable name',
    ).toContain('not set · the bundled face');
  });
});
