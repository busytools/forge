// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it } from 'vitest';

import type { DictateModelsWire } from '../wire/models';
import Models from './Models.svelte';
import ModelsBody from './ModelsBody.svelte';
import { modelsWire } from './testing';

const drawn: ReturnType<typeof mount>[] = [];
const hosts: HTMLElement[] = [];

afterEach(() => {
  for (const component of drawn.splice(0)) void unmount(component);
  for (const host of hosts.splice(0)) host.remove();
});

/** The page as it draws, mounted so a control can be reached. */
function open(wire: DictateModelsWire = modelsWire, oncheck: () => void = () => {}) {
  const host = document.createElement('div');
  document.body.append(host);
  hosts.push(host);
  const component = mount(ModelsBody, { target: host, props: { wire, oncheck } });
  drawn.push(component);
  return host;
}

describe('the models page as it draws', () => {
  /**
   * The two pins, with the facts each side is read from: the pin's own
   * declaration on the first line, the feed's measurement on the second, and
   * the live state as a chip.
   */
  it('draws the models in use', () => {
    const html = open().innerHTML;

    expect(html).toContain('cohere-transcribe-03-2026-Q4_K_M.gguf');
    expect(html).toContain('1.56 GB');
    expect(html).toContain('Q4_K_M');
    expect(html).toContain('sha 0ea56826');
    expect(html).toContain('72.9\u{d7} realtime on m4-max');
    expect(html).toContain('loaded');
    // The normalizer is in no feed and still draws: its own runtime stands
    // where the measurement would be.
    expect(html).toContain('s1-mini-f16.gguf');
    expect(html).toContain('llama.cpp');
    expect(html).toContain('not in the feed');
  });

  /**
   * The proposal's comparison, both sides of both numbers, and the link an
   * adoption takes: the note under it says a proposal becomes a pull request.
   */
  it('draws what the feed proposes', () => {
    const html = open().innerHTML;

    expect(html).toContain('update available');
    expect(html).toContain('Granite Speech 5.0 470M TurboCTC');
    expect(html).toContain('388.8\u{d7} vs 72.9\u{d7}');
    expect(html).toContain('FLEURS-en 4.61 vs 5.08');
    expect(html).toContain('a pull request');
  });

  /**
   * A check that is out is drawn as the core's own state, with no Check now
   * to press: a second check is refused, and a control that reports nothing
   * when pressed reads as broken.
   */
  it('draws a check in flight without the control to start another', () => {
    const host = open({
      ...modelsWire,
      check: { state: 'checking' },
    });

    expect(host.textContent).toContain('checking the catalogue');
    expect(host.querySelector('.status button'), 'a check in flight offers another').toBeNull();
  });

  /** The check's own click is what dispatches, once per press. */
  it('asks for a check when the control is pressed', () => {
    let checks = 0;
    const host = open(modelsWire, () => {
      checks += 1;
    });

    const button = host.querySelector('button');
    expect(button?.textContent).toContain('Check now');
    button?.click();
    flushSync();

    expect(checks).toBe(1);
  });

  /**
   * Finding a model is this side's: the whole feed arrives with the read, so
   * a search filters what is here and asks the server for nothing.
   */
  it('filters the feed on a search', () => {
    const host = open();
    const input = host.querySelector('input');
    expect(input).not.toBeNull();
    // Nothing searched yet: the list is not drawn at all.
    expect(host.textContent).toContain('nothing searched yet');

    if (input !== null) {
      input.value = 'parakeet';
      input.dispatchEvent(new Event('input', { bubbles: true }));
      flushSync();
    }
    host.querySelector('form')?.dispatchEvent(new Event('submit', { cancelable: true }));
    flushSync();

    expect(host.textContent).toContain('parakeet-unified-en-0.6b');
    expect(host.textContent, 'a row nothing matched was drawn').not.toContain(
      'granite-speech-5.0-470m-turboctc',
    );
    expect(host.textContent).toContain('English only');
    expect(host.textContent).toContain('streaming');
  });

  it('says so when nothing matches', () => {
    const host = open();
    const input = host.querySelector('input');
    if (input !== null) {
      input.value = 'wav2vec';
      input.dispatchEvent(new Event('input', { bubbles: true }));
      flushSync();
    }
    host.querySelector('form')?.dispatchEvent(new Event('submit', { cancelable: true }));
    flushSync();

    expect(host.textContent).toContain('no entry matches');
  });

  /**
   * Dictation off is its own state, and the page says which key is unset
   * rather than drawing an empty list that reads as a broken page.
   */
  it('draws the off state when no model is in use', () => {
    // With `[dictate]` off nothing starts the check, so the server's own
    // state for it is `never`.
    const host = open({
      ...modelsWire,
      enabled: false,
      in_use: [],
      updates: [],
      check: { state: 'never' },
    });

    expect(host.textContent).toContain('dictation is off');
    expect(host.textContent).toContain('[dictate] enabled');
    expect(host.textContent).toContain('not checked yet');
    // The feed still crosses, so the search still reads it.
    expect(host.querySelector('input')?.placeholder).toContain('3 variants');
  });

  /**
   * A check state this client does not know says so, and never draws as a
   * fresh one: `up to date` here would be a claim about a feed nothing read.
   */
  it('draws a check state it cannot read as its own state', () => {
    const host = open({ ...modelsWire, check: { state: 'unknown' } as never, updates: [] });

    expect(host.textContent).toContain('this client');
    expect(host.textContent).not.toContain('up to date');
  });

  /** A feed that did not answer carries its own reason, in the error tone. */
  it('draws a check that could not reach the catalogue', () => {
    const host = open({
      ...modelsWire,
      check: { state: 'unreachable', error: 'github.com answered 502' },
      updates: [],
    });

    expect(host.textContent).toContain('the catalogue could not be reached');
    expect(host.textContent).toContain('github.com answered 502');
  });
});

describe('the models route as it draws', () => {
  /** No connection, no read: the page says it is reading rather than drawing
   * an empty catalogue. */
  it('draws the loading line before the first read lands', () => {
    const host = document.createElement('div');
    document.body.append(host);
    hosts.push(host);
    const component = mount(Models, {
      target: host,
      props: { connection: null },
    });
    drawn.push(component);

    expect(host.textContent).toContain('Reading the models...');
    expect(host.querySelector('.models .block')).toBeNull();
  });
});
