// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

import { modelsWire } from '../models/testing';
import type { DictateModelsWire, SpeakingModelWire } from '../wire/models';
import Speaking from './Speaking.svelte';

/** The resolved speaking model the fixture draws, without the assertion
 * every `!` would need. */
const SPEAKING: SpeakingModelWire = {
  variant: 'qwen3-tts-12hz-1.7b-base',
  file: 'Qwen3-TTS-12Hz-1.7B-Base-Q8_0.gguf',
  state: 'ready',
  from: { from: 'pin' },
  at: null,
};

/**
 * The voice area's speaking section: the model the role runs with its one
 * control, and the public corpus's voices with a preview each. The section
 * draws the core's read; the actions go up as callbacks.
 */
let app: Record<string, unknown> | null = null;

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  document.body.innerHTML = '';
});

function draw(over: { wire?: Partial<DictateModelsWire>; listening?: string | null } = {}): {
  oninstall: ReturnType<typeof vi.fn>;
  onactivate: ReturnType<typeof vi.fn>;
  ondeactivate: ReturnType<typeof vi.fn>;
  onpreview: ReturnType<typeof vi.fn>;
} {
  const oninstall = vi.fn();
  const onactivate = vi.fn();
  const ondeactivate = vi.fn();
  const onpreview = vi.fn();
  app = mount(Speaking, {
    target: document.body,
    props: {
      wire: { ...modelsWire, ...over.wire },
      listening: over.listening === undefined ? 'Org/project/lead' : over.listening,
      refusal: null,
      oninstall,
      onactivate,
      ondeactivate,
      onpreview,
    },
  });
  flushSync();
  return { oninstall, onactivate, ondeactivate, onpreview };
}

function buttons(): HTMLButtonElement[] {
  return [...document.querySelectorAll<HTMLButtonElement>('button')];
}

function button(text: string): HTMLButtonElement {
  const found = buttons().find((candidate) => candidate.textContent?.trim() === text);
  if (found === undefined) {
    throw new Error(`no "${text}" control is drawn`);
  }
  return found;
}

describe("the speaking section's model row", () => {
  it('draws the file and the activation control a ready pin offers', () => {
    const { onactivate } = draw();

    expect(document.body.textContent).toContain('Qwen3-TTS-12Hz-1.7B-Base-Q8_0.gguf');
    expect(document.body.textContent).toContain('compiled default');

    button('activate').click();
    expect(onactivate).toHaveBeenCalledWith('Qwen3-TTS-12Hz-1.7B-Base-Q8_0.gguf');
  });

  it('offers the install when the pair is not on disk', () => {
    const { oninstall } = draw({ wire: { speaking: { ...SPEAKING, state: 'pending' } } });

    button('install').click();
    expect(oninstall).toHaveBeenCalledWith('qwen3-tts-12hz-1.7b-base');
  });

  it('offers the way back to the default for a runtime pick', () => {
    const { ondeactivate } = draw({
      wire: { speaking: { ...SPEAKING, from: { from: 'installed', variant: 'qwen3-tts' } } },
    });

    button('use the default').click();
    expect(ondeactivate).toHaveBeenCalled();
  });
});

describe('the speaking section voices', () => {
  it('lists a row per speaker, marks the chosen one, and previews through a seat', () => {
    const { onpreview } = draw();

    expect(document.body.textContent).toContain('6930');
    expect(document.body.textContent).toContain('3575');
    expect(document.body.textContent).toContain('chosen');

    const previews = buttons().filter((candidate) => candidate.textContent?.trim() === 'preview');
    expect(previews).toHaveLength(2);
    expect(previews[0]?.disabled).toBe(false);

    previews[1]?.click();
    expect(onpreview).toHaveBeenCalledWith('3575');
  });

  it('disables every preview until a seat has voice on', () => {
    draw({ listening: null });

    const previews = buttons().filter((candidate) => candidate.textContent?.trim() === 'preview');
    expect(previews).toHaveLength(2);
    expect(previews.every((candidate) => candidate.disabled)).toBe(true);
    expect(document.body.textContent).toContain("turn a seat's voice switch on");
  });

  it('says why a configured voice cannot be used', () => {
    draw({
      wire: { voice_error: '[tts] voice names nobody, which the public corpus does not carry' },
    });

    expect(document.body.textContent).toContain('voice names nobody');
  });
});
