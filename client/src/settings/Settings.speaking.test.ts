// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it } from 'vitest';

import { emptyHome, settingsWire } from '../dev/settings.fixture';
import { modelsWire } from '../models/testing';
import { lastSeat } from '../routes';
import type { Connection } from '../socket';
import { resetVoices } from '../voice/registry';
import type { DictateModelsWire, SpeakingFrom } from '../wire/models';
import Settings from './Settings.svelte';
import { settingsFrame } from './frame';

/**
 * The voice area's speaking card: the model controls, the voices the model
 * carries, and the preview, which plays on the seat the reader was last on
 * through that seat's own stream.
 */
let app: Record<string, unknown> | null = null;

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  settingsFrame.set({ status: 'waiting' });
  lastSeat.set(null);
  resetVoices();
  document.body.innerHTML = '';
});

function connection(
  seen: { dispatched: unknown[]; edits: { key_path: string }[] },
  wire: DictateModelsWire = modelsWire,
): Connection {
  return {
    onMessage: () => () => {},
    settingsEdit: (_fingerprint: string, edits: { key_path: string }[]) => {
      seen.edits.push(...edits);
      return true;
    },
    dispatch: (command: unknown) => {
      seen.dispatched.push(command);
      return Promise.resolve();
    },
    subscribe: () => ({
      subject: 'dictate_models',
      state: () => ({ kind: 'ready' }),
      snapshot: () => wire,
      updates: [],
      dropped: 0,
    }),
    // The settings page pumps the last seat's voice while it is open; this
    // page's fake holds no seat store.
    store: () => undefined,
    unsubscribe: () => {},
    refresh: () => true,
    onStatus: () => () => {},
  } as unknown as Connection;
}

/** The speaking read with its active variant's install switched, for the
 * states the row's controls are keyed on. */
function speakingWire(installed: boolean, from: SpeakingFrom): DictateModelsWire {
  const active = modelsWire.speaking.active;
  return {
    ...modelsWire,
    speaking: {
      ...modelsWire.speaking,
      active: active === null ? null : { ...active, installed },
      from,
    },
  };
}

function draw(open: Connection): void {
  settingsFrame.set({ status: 'ready', document: settingsWire });
  app = mount(Settings, {
    target: document.body,
    props: { connection: open, home: emptyHome, mark: null },
  });
  flushSync();
  const rail = [...document.querySelectorAll<HTMLButtonElement>('.settings-rail button')];
  rail
    .find((button) => button.querySelector('.settings-rail-name')?.textContent?.trim() === 'voice')
    ?.click();
  flushSync();
}

function voiceRow(id: string): HTMLLIElement {
  const row = [...document.querySelectorAll<HTMLLIElement>('.settings-speaking li')].find((item) =>
    item.textContent?.includes(id),
  );
  expect(row, `the ${id} row draws`).not.toBeUndefined();
  return row as HTMLLIElement;
}

/** The page's words with their source line breaks folded, so a phrase that
 * wraps in the markup still reads as one string. */
function bodyText(): string {
  return (document.body.textContent ?? '').replace(/\s+/g, ' ');
}

function press(where: HTMLElement, label: string): void {
  const button = [...where.querySelectorAll<HTMLButtonElement>('button')].find(
    (candidate) => candidate.textContent?.trim() === label,
  );
  expect(button, `the ${label} control draws`).not.toBeUndefined();
  button?.click();
  flushSync();
}

describe("the settings page's speaking card", () => {
  it('draws the model row and the voices the model carries', () => {
    draw(connection({ dispatched: [], edits: [] }));

    expect(bodyText()).toContain('hexgrad/Kokoro-82M');
    expect(bodyText()).toContain('kokoro-82m-int8');
    expect(bodyText()).toContain('active');
    expect(bodyText()).toContain('every reply renders in the chosen voice (bf_emma)');
    expect(voiceRow('af_heart')).not.toBeNull();
    expect(voiceRow('bf_emma')).not.toBeNull();
  });

  it('previews a voice on the seat the reader was last on', () => {
    const seen = { dispatched: [] as unknown[], edits: [] as { key_path: string }[] };
    lastSeat.set({ org: 'Busytools', project: 'forge', label: 'lead' });
    draw(connection(seen));

    press(voiceRow('af_heart'), 'preview');

    expect(seen.dispatched).toEqual([
      {
        voice_preview: {
          key: { org: 'Busytools', project: 'forge', label: 'lead' },
          voice: 'af_heart',
        },
      },
    ]);
  });

  /** A preview outside a session has nowhere to play, so the card says so
   * rather than offering a control the core would refuse. */
  it('disables the preview, and says why, before a seat was opened', () => {
    draw(connection({ dispatched: [], edits: [] }));

    const button = [...voiceRow('af_heart').querySelectorAll<HTMLButtonElement>('button')].find(
      (candidate) => candidate.textContent?.trim() === 'preview',
    );
    expect(button?.disabled).toBe(true);
    expect(bodyText()).toContain('open a session and turn the speaker on');
  });

  /** A render that fails after the command was accepted is the core's
   * failure, not a refusal, so it arrives on the speaking read and is drawn
   * under the card. */
  it('draws a preview render failure the read carries', () => {
    const wire = {
      ...modelsWire,
      speaking: { ...modelsWire.speaking, preview_error: 'the render died' },
    };
    draw(connection({ dispatched: [], edits: [] }, wire));

    expect(bodyText()).toContain('the render died');
  });

  /** The choice is the page's own edit: staged by the card, written by the
   * one Save, so there is no second writer for `[tts] voice`. */
  it('stages the chosen voice and sends it through the page save', () => {
    const seen = { dispatched: [] as unknown[], edits: [] as { key_path: string }[] };
    draw(connection(seen));

    press(voiceRow('af_heart'), 'use');
    expect(document.querySelector('.settings-dock'), 'the edit bar opens').not.toBeNull();

    const save = [...document.querySelectorAll<HTMLButtonElement>('button')].find(
      (button) => button.textContent?.trim() === 'save',
    );
    save?.click();
    flushSync();

    expect(seen.edits.map((edit) => edit.key_path)).toContain('tts.voice');
  });

  it('installs the model when the machine does not have it', () => {
    const seen = { dispatched: [] as unknown[], edits: [] as { key_path: string }[] };
    draw(connection(seen, speakingWire(false, 'pin')));

    press(document.body, 'install int8');

    expect(seen.dispatched).toEqual([{ dictate_install: { variant: 'kokoro-82m-int8' } }]);
  });

  /** An installed compiled default offers the activation that makes it the
   * runtime pick. */
  it('activates an installed default', () => {
    const seen = { dispatched: [] as unknown[], edits: [] as { key_path: string }[] };
    draw(connection(seen, speakingWire(true, 'pin')));

    press(document.body, 'activate');

    expect(seen.dispatched).toEqual([
      { dictate_activate: { role: 'speaking', file: 'model_quantized.onnx' } },
    ]);
  });
});
