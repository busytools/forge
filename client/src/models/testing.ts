/**
 * The models page's wire and a connection that drives it, both for tests.
 *
 * **Nothing the shipped app imports may import this file.** It is mock data
 * and a fake server standing where real ones stand, and the app's only input
 * is the server URL. `fixture.test.ts` builds the bundle and fails on a
 * fixture reaching it - by a marker string, and this file carries none (the
 * models wire names no org). So the rule here is checked directly instead:
 * `testing.test.ts` sweeps the tree and fails if anything outside a test
 * imports this module.
 */

import type { Command, ServerMessage, Subject } from '../protocol';
import type { Connection, ConnectionStatus } from '../socket';
import { Stores } from '../stores';
import { modelsFrom, type DictateModelsWire } from '../wire/models';

/** One catalogue row, from the feed's own published figures. */
function row(
  variant: string,
  display: string,
  family: string,
  params: number,
  license: string,
  size: number,
  speed: number,
  wer: number,
  languages: string[],
  streaming: boolean,
) {
  return {
    variant,
    display_name: display,
    family,
    params,
    license,
    languages,
    streaming,
    download: { quant: 'Q4_K_M', size_bytes: size },
    speed: { machine: 'm4-max', backend: 'metal', quant: 'Q8_0', xrt_wall: speed },
    wer: { dataset: 'fleurs', split: 'test', language: 'en', err_pct: wer },
  };
}

/** What a forge with both pins ready, a fresh feed and one proposal answers. */
export const modelsWire: DictateModelsWire = modelsFrom({
  enabled: true,
  models_dir: '/Users/ved/Library/Caches/forge-dictate',
  in_use: [
    {
      role: 'transcribing',
      file: 'cohere-transcribe-03-2026-Q4_K_M.gguf',
      size: 1_558_162_944,
      sha256: '0ea56826d8bd5d74b7143a4a04e022dc1bb75452cfae49d98b6acb0c1d16a1fb',
      state: 'ready',
      facts: {
        quant: 'Q4_K_M',
        params: 2_049_026_832,
        license: 'Apache-2.0',
        runtime: 'transcribe.cpp',
      },
      catalogue: {
        variant: 'cohere-transcribe-03-2026',
        display_name: 'Cohere Transcribe',
        size_bytes: 1_558_162_944,
        streaming: false,
        languages: ['en', 'fr', 'de', 'es'],
        speed: { machine: 'm4-max', backend: 'metal', quant: 'Q8_0', xrt_wall: 72.9 },
      },
    },
    {
      role: 'normalization',
      file: 's1-mini-f16.gguf',
      size: 1_509_347_232,
      sha256: '0370da4f1bae19e3150bcafa33c5d396c15f97bf25519540a3e013db5cc00af4',
      state: 'ready',
      facts: {
        quant: 'F16',
        params: 596_000_000,
        license: 'Apache-2.0 + naming clause',
        runtime: 'llama.cpp',
      },
      catalogue: null,
    },
  ],
  check: { state: 'fresh', at: '2026-10-06T06:12:00Z', release: 'v0.3.1', skipped: 0 },
  updates: [
    {
      role: 'transcribing',
      file: 'cohere-transcribe-03-2026-Q4_K_M.gguf',
      current: { speed_x: 72.9, fleurs_en_wer: 5.08 },
      candidate: row(
        'granite-speech-5.0-470m-turboctc',
        'Granite Speech 5.0 470M TurboCTC',
        'granite',
        470_000_000,
        'Apache-2.0',
        279_000_000,
        388.8,
        4.61,
        ['en'],
        false,
      ),
    },
  ],
  rows: [
    row(
      'granite-speech-5.0-470m-turboctc',
      'Granite Speech 5.0 470M TurboCTC',
      'granite',
      470_000_000,
      'Apache-2.0',
      279_000_000,
      388.8,
      4.61,
      ['en'],
      false,
    ),
    // The feed carries the non-commercial twin too, and the page lists it:
    // only a PROPOSAL excludes one, which is the server's rule.
    row(
      'granite-speech-5.0-470m-turboctc-nc',
      'Granite Speech 5.0 470M TurboCTC NC',
      'granite',
      473_014_752,
      'CC-BY-NC-SA-4.0',
      279_000_000,
      401.6,
      4.3,
      ['en'],
      false,
    ),
    row(
      'parakeet-unified-en-0.6b',
      'Parakeet Unified EN',
      'parakeet',
      600_000_000,
      'CC-BY-4.0',
      477_000_000,
      218.8,
      3.99,
      ['en'],
      true,
    ),
    row(
      'qwen3-asr-1.7b',
      'Qwen3 ASR',
      'qwen',
      1_700_000_000,
      'Apache-2.0',
      1_320_000_000,
      51.3,
      3.23,
      ['en', 'zh', 'fr'],
      false,
    ),
  ],
} as unknown as DictateModelsWire);

/**
 * A connection a test drives by hand: what the page asked it to do, and a way
 * to hand it a frame.
 *
 * The stores are the real ones, because a subscription's lifecycle is what
 * this fakes around rather than anything about a store - and `arrive` writes
 * a snapshot into its store the way the socket does, so a read through the
 * store is the answer the server gave.
 */
export function fakeConnection() {
  const stores = new Stores();
  const subscribed: Subject[] = [];
  const unsubscribed: Subject[] = [];
  const refreshed: Subject[] = [];
  const dispatched: Command[] = [];
  const messages = new Set<(message: ServerMessage) => void>();
  const statuses = new Set<(status: ConnectionStatus) => void>();

  const connection: Connection = {
    subscribe(what) {
      subscribed.push(what);
      return stores.open(what);
    },
    unsubscribe(what) {
      unsubscribed.push(what);
      return stores.close(what);
    },
    refresh(what) {
      refreshed.push(what);
    },
    dispatch(command: Command) {
      dispatched.push(command);
      return null;
    },
    onMessage(fn) {
      messages.add(fn);
      return () => {
        messages.delete(fn);
      };
    },
    onStatus(fn) {
      statuses.add(fn);
      return () => {
        statuses.delete(fn);
      };
    },
    more: () => false,
    devices: () => false,
    frame: () => false,
    store: () => undefined,
    settings: () => null,
    skew: () => null,
    status: () => 'open',
    close: () => {},
    onBrowserAsk: () => () => {},
    browserRole: () => false,
    onBrowserRole: () => () => {},
    takeBrowserRole: () => {},
  };

  return {
    connection,
    subscribed,
    unsubscribed,
    refreshed,
    dispatched,
    /** Everything still attached to the connection. */
    listening: () => messages.size + statuses.size,
    /**
     * One frame as the server sent it: the store is written the way the
     * socket writes it first, so a read of it is the answer the server gave,
     * and then every listener hears it.
     */
    arrive(message: ServerMessage) {
      if (message.kind === 'snapshot') {
        stores.get(message.subject)?.set(message.data);
      }
      if (message.kind === 'error' && message.what === 'subscribe') {
        stores.get(MODELS)?.refuse(message.why);
      }
      for (const fn of [...messages]) fn(message);
    },
  };
}

/** The models page's subject, which only this page watches. */
export const MODELS: Subject = 'dictate_models';
