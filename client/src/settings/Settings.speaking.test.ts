// @vitest-environment jsdom
import { readFileSync } from 'node:fs';

import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

import { boxKey } from '../composer/box.svelte';
import { emptyHome, settingsWire } from '../dev/settings.fixture';
import { modelsWire } from '../models/testing';
import { lastSeat } from '../routes';
import type { Connection } from '../socket';
import { resetVoices, voiceFor } from '../voice/registry';
import { Voice, connectionPort } from '../voice/voice.svelte';
import type { DictateModelsWire, SpeakingFrom } from '../wire/models';
import type { SettingsDocument, SettingsValue } from '../wire/settings';
import Settings from './Settings.svelte';
import SpeakingCard from './SpeakingCard.svelte';
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
  vi.restoreAllMocks();
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

/** The settings document with one key's value replaced, so a test can put the
 * saved file and the running role in disagreement. */
function settingsWith(path: string, value: SettingsValue): SettingsDocument {
  return {
    ...settingsWire,
    areas: settingsWire.areas.map((area) => ({
      ...area,
      keys: area.keys.map((entry) => (entry.path === path ? { ...entry, value } : entry)),
    })),
  };
}

function draw(open: Connection, sheet: SettingsDocument = settingsWire): void {
  settingsFrame.set({ status: 'ready', document: sheet });
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

/** The voices fold starts shut, so a test that reaches a voice opens it. The
 * summary's own click is the browser's toggle; jsdom leaves that to the test,
 * which moves `open` and fires what the platform fires. */
function setVoicesOpen(open: boolean): HTMLDetailsElement {
  const fold = document.querySelector<HTMLDetailsElement>(
    '.settings-speaking details.settings-card',
  );
  expect(fold, 'the voices card draws as a foldable card').not.toBeNull();
  const at = fold as HTMLDetailsElement;
  if (at.open !== open) {
    at.open = open;
    at.dispatchEvent(new Event('toggle'));
    flushSync();
  }
  return at;
}

function openVoices(): HTMLDetailsElement {
  return setVoicesOpen(true);
}

const SEAT = { org: 'Busytools', project: 'forge', label: 'lead' };

/** This client's speaker switch on for the seat a preview would play on: the
 * card withholds a preview until it is, because the core has no listener. */
function listen(open: Connection): void {
  voiceFor(boxKey(SEAT), connectionPort(open, SEAT)).turnOn();
  flushSync();
}

const SHEET = readFileSync('src/assets/web.css', 'utf8');

/** The rule body the sheet writes for this exact selector, comments gone. */
function ruleFor(selector: string): string {
  const uncommented = SHEET.replace(/\/\*[\s\S]*?\*\//g, ' ');
  const escaped = selector.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  const match = new RegExp(`^\\s*${escaped}\\s*\\{([^}]*)\\}`, 'gm').exec(uncommented);
  return (match?.[1] ?? '').replace(/\s+/g, ' ').trim();
}

function voiceRow(id: string): HTMLElement {
  const row = [
    ...document.querySelectorAll<HTMLElement>('.settings-speaking .settings-setting'),
  ].find((item) => item.querySelector('.settings-setting-label code')?.textContent?.trim() === id);
  expect(row, `the ${id} row draws`).not.toBeUndefined();
  return row as HTMLElement;
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

/** The card on its own, for the states the page's audio cannot reach in
 * jsdom (there is no AudioContext to play a clip through). */
function drawCard(
  preview: { voice: string; phase: 'working' | 'playing' | 'failed' } | null,
  previewFailure: string | null = null,
  onstoppreview: () => void = () => {},
): void {
  app = mount(SpeakingCard, {
    target: document.body,
    props: {
      speaking: modelsWire.speaking,
      savedEnabled: true,
      install: { state: 'idle' },
      activate: { state: 'idle' },
      busy: false,
      voice: 'bf_emma',
      seat: SEAT,
      listening: true,
      preview,
      previewFailure,
      oninstall: () => {},
      onactivate: () => {},
      ondeactivate: () => {},
      onchoose: () => {},
      onpreview: () => {},
      onstoppreview,
    },
  });
  flushSync();
  openVoices();
}

describe("the settings page's speaking card", () => {
  it('draws the model row and the voices the model carries', () => {
    draw(connection({ dispatched: [], edits: [] }));
    openVoices();

    expect(bodyText()).toContain('hexgrad/Kokoro-82M');
    expect(bodyText()).toContain('kokoro-82m-int8');
    expect(bodyText()).toContain('active');
    expect(bodyText()).toContain('the chosen voice is bf_emma');
    expect(voiceRow('af_heart')).not.toBeNull();
    expect(voiceRow('bf_emma')).not.toBeNull();
  });

  /** Fifty-four rows are most of a screen: the section arrives shut, its
   * head band is the page's card head, and that head carries the chevron. */
  it('draws the voices as a card whose head is the disclosure', () => {
    draw(connection({ dispatched: [], edits: [] }));

    const fold = document.querySelector<HTMLDetailsElement>(
      '.settings-speaking details.settings-card',
    );
    expect(fold, 'the voices card draws').not.toBeNull();
    const at = fold as HTMLDetailsElement;
    expect(at.open, 'the card starts shut').toBe(false);
    const head = at.querySelector('summary.settings-card-head');
    expect(head, 'the card head is the summary').not.toBeNull();
    expect(head?.querySelector('h3')?.textContent?.trim()).toBe('voices');
    expect(head?.querySelector('.settings-card-count')?.textContent?.trim()).toBe('2');
    expect(head?.querySelector('.arw'), 'the chevron draws shut too').not.toBeNull();
    expect(
      ruleFor('.settings-page details.settings-card > summary .arw'),
      'the chevron rests visible rather than behind a hover',
    ).toContain('opacity: 1');

    head?.dispatchEvent(new MouseEvent('click', { bubbles: true }));
    flushSync();
    expect(at.open, 'the head opens it').toBe(true);
    head?.dispatchEvent(new MouseEvent('click', { bubbles: true }));
    flushSync();
    expect(at.open, 'and closes it').toBe(false);
  });

  /** The list is grouped by the language each voice prefix belongs to: one
   * voice per prefix proves every language's prefix set, and the per-group
   * query proves a voice is not sitting under the wrong heading. */
  it('groups every voice under its language, exactly once', () => {
    const byLanguage: [string, string[]][] = [
      ['English', ['af_heart', 'am_adam', 'bf_emma', 'bm_daniel']],
      ['Spanish', ['ef_dora', 'em_alex']],
      ['French', ['ff_siwis']],
      ['Hindi', ['hf_alpha', 'hm_omega']],
      ['Italian', ['if_sara', 'im_nicola']],
      ['Japanese', ['jf_alpha', 'jm_kumo']],
      ['Portuguese', ['pf_dora', 'pm_alex']],
      ['Chinese', ['zf_xiaobei', 'zm_yunjian']],
    ];
    const voices = byLanguage.flatMap(([, ids]) => ids);
    const wire = { ...modelsWire, speaking: { ...modelsWire.speaking, voices } };
    draw(connection({ dispatched: [], edits: [] }, wire));
    openVoices();

    for (const [language, ids] of byLanguage) {
      const group = document.querySelector(`.voice-rows[aria-label="Voices, ${language}"]`);
      expect(group, `the ${language} group draws`).not.toBeNull();
      const drawn = [
        ...(group as HTMLElement).querySelectorAll('.settings-setting-label code'),
      ].map((name) => name.textContent?.trim());
      expect(drawn, `${language} holds exactly its own voices`).toEqual(ids);
    }
    expect(
      document.querySelectorAll('.settings-speaking .voice-rows > .settings-setting').length,
      'every voice draws exactly once',
    ).toBe(voices.length);
  });

  it('previews a voice on the seat the reader was last on', () => {
    const seen = { dispatched: [] as unknown[], edits: [] as { key_path: string }[] };
    lastSeat.set(SEAT);
    const open = connection(seen);
    draw(open);
    openVoices();
    listen(open);
    seen.dispatched.length = 0;

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

  /** One preview at a time: a press stops the clip sounding now before it
   * asks for the next voice, so the new one is heard as its first chunk
   * arrives rather than queued behind the old clip. */
  it('wakes the device, stops the sounding preview, then asks for the next voice', () => {
    const order: string[] = [];
    vi.spyOn(Voice.prototype, 'unlock').mockImplementation(() => {
      order.push('unlock');
    });
    vi.spyOn(Voice.prototype, 'stop').mockImplementation(() => {
      order.push('stop');
    });
    const seen = { dispatched: [] as unknown[], edits: [] as { key_path: string }[] };
    lastSeat.set({ org: 'Busytools', project: 'forge', label: 'lead' });
    const open = connection(seen);
    const dispatch = open.dispatch.bind(open);
    open.dispatch = (command: Parameters<Connection['dispatch']>[0]) => {
      order.push('dispatch');
      return dispatch(command);
    };
    draw(open);
    openVoices();
    listen(open);
    seen.dispatched.length = 0;
    order.length = 0;

    press(voiceRow('af_heart'), 'preview');
    expect(order, 'the wake and stop come before the next voice is asked for').toEqual([
      'unlock',
      'stop',
      'dispatch',
    ]);
    press(voiceRow('bf_emma'), 'preview');
    expect(order).toEqual(['unlock', 'stop', 'dispatch', 'unlock', 'stop', 'dispatch']);
  });

  /** The playing state lasts the clip's own duration: a state and a way to
   * stop it, in the row's own chip vocabulary. */
  it('draws a playing preview with its stop on the row', () => {
    let stopped = 0;
    drawCard({ voice: 'af_heart', phase: 'playing' }, null, () => {
      stopped += 1;
    });

    const row = voiceRow('af_heart');
    expect(row.textContent).toContain('playing');
    expect(row.classList.contains('playing'), 'the sounding row is marked').toBe(true);
    press(row, 'stop');
    expect(stopped).toBe(1);
  });

  /** A failed preview names its failure on the row, with the preview door
   * back so the press can be tried again. */
  it('draws a failed preview in place on the row', () => {
    drawCard({ voice: 'af_heart', phase: 'failed' }, 'the render died');

    const row = voiceRow('af_heart');
    expect(row.textContent).toContain('the render died');
    expect(
      [...row.querySelectorAll('button')].map((button) => button.textContent?.trim()),
    ).toContain('preview');
  });

  /** A seat this client's switch is off on has nowhere to play either: the
   * core counts listeners, and another client listening is not this one. */
  it('disables the preview until this client turns the seat speaker on', () => {
    lastSeat.set(SEAT);
    draw(connection({ dispatched: [], edits: [] }));
    openVoices();

    const button = [...voiceRow('af_heart').querySelectorAll<HTMLButtonElement>('button')].find(
      (candidate) => candidate.textContent?.trim() === 'preview',
    );
    expect(button?.disabled).toBe(true);
    expect(bodyText()).toContain('turn the speaker on');
  });

  /** A press shows on the row at once: the control becomes a disabled
   * preparing label until the clip's first chunk sounds. */
  it('draws the pressed row as preparing while the render runs', () => {
    const seen = { dispatched: [] as unknown[], edits: [] as { key_path: string }[] };
    lastSeat.set(SEAT);
    const open = connection(seen);
    draw(open);
    openVoices();
    listen(open);
    seen.dispatched.length = 0;

    press(voiceRow('af_heart'), 'preview');

    const labels = [...voiceRow('af_heart').querySelectorAll<HTMLButtonElement>('button')].map(
      (button) => ({ label: button.textContent?.trim(), disabled: button.disabled }),
    );
    expect(labels).toContainEqual({ label: 'preparing', disabled: true });
    expect(labels.map((button) => button.label)).not.toContain('preview');
  });

  /** A second press moves the row state in one step: the old row is no
   * longer preparing or playing, the new one is. */
  it('moves the pressed state to the new row in one step', () => {
    const seen = { dispatched: [] as unknown[], edits: [] as { key_path: string }[] };
    lastSeat.set(SEAT);
    const open = connection(seen);
    draw(open);
    openVoices();
    listen(open);
    seen.dispatched.length = 0;

    press(voiceRow('af_heart'), 'preview');
    expect(voiceRow('af_heart').textContent).toContain('preparing');

    press(voiceRow('bf_emma'), 'preview');

    expect(voiceRow('bf_emma').textContent).toContain('preparing');
    expect(voiceRow('af_heart').textContent, 'the old row is cleared').not.toContain('preparing');
    expect(
      [...voiceRow('af_heart').querySelectorAll('button')].map((button) =>
        button.textContent?.trim(),
      ),
      'and offers its preview again',
    ).toContain('preview');
  });

  /** A preview outside a session has nowhere to play, so the card says so
   * rather than offering a control the core would refuse. */
  it('disables the preview, and says why, before a seat was opened', () => {
    draw(connection({ dispatched: [], edits: [] }));
    openVoices();

    const button = [...voiceRow('af_heart').querySelectorAll<HTMLButtonElement>('button')].find(
      (candidate) => candidate.textContent?.trim() === 'preview',
    );
    expect(button?.disabled).toBe(true);
    expect(bodyText()).toContain('open a session and turn the speaker on');
  });

  /** `[tts] enabled` unset is a state the page draws: the core refuses an
   * install or a preview while speech is off, so the card says why and
   * withholds both controls rather than offering a press that always errors. */
  it('says speech is off and withholds the model and preview controls', () => {
    const wire = { ...modelsWire, speaking: { ...modelsWire.speaking, enabled: false } };
    draw(
      connection({ dispatched: [], edits: [] }, wire),
      settingsWith('tts.enabled', { kind: 'bool', text: 'false' }),
    );

    expect(bodyText()).toContain('speech is off');
    const labels = [
      ...document.querySelectorAll<HTMLButtonElement>('.settings-speaking button'),
    ].map((button) => button.textContent?.trim());
    expect(labels).not.toContain('use the default');
    expect(labels).not.toContain('preview');
  });

  /** `[tts] enabled` is restart-class: the file and the running role can
   * disagree until a restart, and each control follows the half that answers
   * it - install and switch read the file, a preview needs the running role. */
  it('names the restart when the saved switch and the running role disagree', () => {
    const runningOff = { ...modelsWire, speaking: { ...modelsWire.speaking, enabled: false } };
    draw(connection({ dispatched: [], edits: [] }, runningOff));
    expect(bodyText()).toContain('speech is saved on and the running role is off');
    expect(bodyText()).toContain('applies after a restart');
    const labelsOff = [
      ...document.querySelectorAll<HTMLButtonElement>('.settings-speaking button'),
    ].map((button) => button.textContent?.trim());
    expect(labelsOff, 'the file gates the switch').toContain('use the default');
    expect(labelsOff, 'the running role gates the preview').not.toContain('preview');
  });

  it('draws a saved-off switch that still runs as the restart state', () => {
    draw(
      connection({ dispatched: [], edits: [] }),
      settingsWith('tts.enabled', { kind: 'bool', text: 'false' }),
    );
    expect(bodyText()).toContain('speech is saved off and the running role is on');
    const labelsOn = [
      ...document.querySelectorAll<HTMLButtonElement>('.settings-speaking button'),
    ].map((button) => button.textContent?.trim());
    expect(labelsOn, 'the file refuses the switch').not.toContain('use the default');
    expect(labelsOn, 'the running role still previews').toContain('preview');
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
    openVoices();

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
