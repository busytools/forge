/**
 * The models page's wire, built by hand for tests.
 *
 * **Nothing the shipped app imports may import this file.** It is mock data
 * standing where a server's answer stands; the app's only input is the server
 * URL, and `fixture.test.ts` builds the bundle and fails if a fixture reaches
 * it.
 */

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
