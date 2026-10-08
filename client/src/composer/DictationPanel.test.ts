// @vitest-environment jsdom
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
