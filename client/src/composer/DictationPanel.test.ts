// @vitest-environment jsdom
import { readFileSync } from 'node:fs';

import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

import { fakeConnection, MODELS, modelsWire } from '../models/testing';
import { DEFAULT_AXES } from '../session/wire';
import DictationPanel from './DictationPanel.svelte';

/**
 * The panel's own vocabulary.
 *
 * The device rows borrowed the sheet's bare `.row` - the home list's name - so
 * a property added to that rule reached the panel's buttons with nothing to say
 * so (#1727). The rows carry their own name now, and this pins it: a revert, or
 * a row drawn with the shared name beside it, dies here.
 */
vi.mock('./mic', async (importOriginal) => {
  const original = await importOriginal<typeof import('./mic')>();
  return {
    ...original,
    inputs: () =>
      Promise.resolve([
        { id: 'a', label: 'Shure SM7B' },
        { id: 'b', label: '' },
      ]),
  };
});

// A path rather than a URL: this file runs under jsdom, where `import.meta.url`
// is the dev server's and not a file the disk can be read at.
const sheet = readFileSync('src/assets/web.css', 'utf8');

/** The last rule body the sheet writes for this exact selector. */
function sheetRule(selector: string): string {
  const escaped = selector.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  const found = [...sheet.matchAll(new RegExp(`^\\s*${escaped}\\s*\\{([^}]*)\\}`, 'gm'))];
  const body = found.at(-1)?.[1];
  // A miss must not read as an empty rule: a not.toContain leg would pass on it.
  if (body === undefined) throw new Error(`the sheet writes no rule for ${selector}`);
  return body;
}

let app: Record<string, unknown> | null = null;

afterEach(() => {
  if (app !== null) void unmount(app);
  app = null;
  document.body.innerHTML = '';
});

/** The panel with its device list walked open. */
async function drawn(): Promise<void> {
  app = mount(DictationPanel, {
    target: document.body,
    props: {
      axes: DEFAULT_AXES,
      defaults: DEFAULT_AXES,
      bind: 'right_cmd',
      mode: 'auto',
      device: null,
      onaxes: () => undefined,
      ondevice: () => undefined,
      onclose: () => undefined,
    },
  });
  flushSync();
  const trigger = document.querySelector<HTMLButtonElement>('.pop .dev');
  if (trigger === null) throw new Error('the panel drew no input-device trigger');
  trigger.click();
  await vi.waitFor(() => {
    if (document.querySelector('.pop .list button') === null) {
      throw new Error('the walk has not landed');
    }
  });
}

describe('the panel rows', () => {
  /**
   * **The device door's mark answers its own state.** It drew a literal
   * chevron that said the same thing open and closed, while every other
   * disclosure on the page draws the shared arrow, turned by the state the
   * button already announces (#1504).
   */
  it('turns the device door arrow by the open state it announces', async () => {
    await drawn();

    const trigger = document.querySelector<HTMLButtonElement>('.pop .dev');
    if (trigger === null) throw new Error('the panel drew no input-device trigger');
    expect(trigger.getAttribute('aria-expanded'), 'the walk left the list open').toBe('true');
    expect(trigger.querySelector('svg.arw'), 'the shared arrow, not a literal glyph').not.toBeNull();
    expect(trigger.textContent, 'and no literal chevron beside it').not.toContain('▸');
    const turned = sheetRule('.pop .dev[aria-expanded="true"] .arw');
    expect(turned, 'the sheet turns the arrow by the state the button announces').toContain(
      'rotate(90deg)',
    );
  });

  it("carry their own name, not the sheet's bare one", async () => {
    await drawn();

    const rows = [...document.querySelectorAll('.pop .list button')];
    expect(rows.length, 'the open list drew no rows to check').toBe(3);
    for (const row of rows) {
      expect(row.classList.contains('drow'), "a device row lost the panel's own class").toBe(true);
      expect(row.classList.contains('row'), 'a device row carries the shared bare name again').toBe(
        false,
      );
    }
  });
});

describe('the model rows', () => {
  /**
   * **The panel says which model each role holds, off the same read the
   * models page draws**, and its press is that page's own activation. A
   * panel that drew nothing while the read was out would read as a session
   * with no models rather than one still being read.
   */
  it('draws each role from the read, and the press activates', async () => {
    const forge = fakeConnection();
    app = mount(DictationPanel, {
      target: document.body,
      props: {
        axes: DEFAULT_AXES,
        defaults: DEFAULT_AXES,
        bind: 'right_cmd',
        mode: 'auto',
        device: null,
        onaxes: () => undefined,
        ondevice: () => undefined,
        onclose: () => undefined,
        connection: forge.connection,
      },
    });
    flushSync();
    expect(document.body.textContent).toContain('reading the models this session runs');

    forge.arrive({ kind: 'snapshot', subject: MODELS, data: modelsWire });
    await vi.waitFor(() => {
      if (!document.body.textContent?.includes('cohere-transcribe-03-2026-Q4_K_M.gguf')) {
        throw new Error('the read has not landed');
      }
    });
    expect(document.body.textContent).toContain('s1-mini-f16.gguf');

    const dev = [...document.querySelectorAll<HTMLButtonElement>('.pop .dev')].find((button) =>
      button.textContent?.includes('cohere-transcribe'),
    );
    expect(dev, 'the transcribing row drew no door').not.toBeUndefined();
    dev?.click();
    flushSync();

    const choice = [...document.querySelectorAll<HTMLButtonElement>('.pop .list .drow')].find(
      (button) => button.textContent?.includes('granite-speech-5.0-470m-turboctc-nc'),
    );
    expect(choice, 'the opened list drew no choices').not.toBeUndefined();
    choice?.click();
    flushSync();

    expect(forge.dispatched).toContainEqual({
      dictate_activate: {
        role: 'transcribing',
        file: 'granite-speech-5.0-470m-turboctc-nc-Q4_K_M.gguf',
      },
    });
  });
});

/**
 * **A panel with no row of its own hangs off the host's PADDING box**, which is
 * what the `bottom: calc(100% + 6px)` this replaced measured against. The
 * review's thread card is a bordered host, and a border-box measurement hangs
 * the panel a border's height too high (#2527's gate).
 */
describe('the panel without a row above it', () => {
  it("hangs off the host's padding box, not its border box", () => {
    const host = document.createElement('div');
    document.body.append(host);
    Object.defineProperty(host, 'getBoundingClientRect', {
      configurable: true,
      value: () => ({
        top: 100,
        bottom: 700,
        height: 600,
        left: 0,
        right: 0,
        width: 0,
        x: 0,
        y: 100,
        toJSON: () => ({}),
      }),
    });
    // A 2px border, as the thread card draws.
    Object.defineProperty(host, 'clientTop', { configurable: true, value: 2 });
    Object.defineProperty(host, 'clientHeight', { configurable: true, value: 596 });

    app = mount(DictationPanel, {
      target: host,
      props: {
        axes: DEFAULT_AXES,
        defaults: DEFAULT_AXES,
        bind: 'right_cmd',
        mode: 'auto',
        device: null,
        onaxes: () => undefined,
        ondevice: () => undefined,
        onclose: () => undefined,
      },
    });
    flushSync();

    const pop = document.querySelector('.pop');
    if (!(pop instanceof HTMLElement)) throw new Error('the panel drew nothing to place');
    expect(pop.style.bottom, 'the hang ignored the border').toBe('602px');
    expect(pop.style.maxHeight, 'the room counted the border').toBe('90px');
  });
});
