import { describe, expect, it } from 'vitest';

import { modelsFrom, type DictateModelsWire, type InUseModel } from './models';

/**
 * A payload of the shape `crates/forge-workspace/src/catalogue.rs` writes,
 * with the members a narrowing test cares about left overridable.
 */
function payload(over: Partial<Record<string, unknown>> = {}): DictateModelsWire {
  return {
    enabled: true,
    models_dir: '/tmp/models',
    in_use: [
      {
        role: 'transcribing',
        file: 'cohere-transcribe-03-2026-Q4_K_M.gguf',
        size: 1_558_162_944,
        sha256: '0ea56826',
        state: 'ready',
        facts: {
          quant: 'Q4_K_M',
          params: 2_049_026_832,
          license: 'Apache-2.0',
          runtime: 'transcribe.cpp',
        },
        catalogue: null,
      },
      {
        role: 'normalization',
        file: 's1-mini-f16.gguf',
        size: 1_509_347_232,
        sha256: '0370da4f',
        state: 'loading',
        facts: { quant: 'F16', params: 596_000_000, license: 'Apache-2.0', runtime: 'llama.cpp' },
        catalogue: null,
      },
    ],
    check: { state: 'fresh', at: '2026-10-06T06:12:00Z', release: 'v0.3.1', skipped: 2 },
    updates: [],
    rows: [],
    ...over,
  } as unknown as DictateModelsWire;
}

describe("the models page's snapshot, narrowed once where it enters", () => {
  /**
   * The check state is the page's one union of literals, and `checkLine`
   * switches over it exhaustively. An unknown tag is a client older than its
   * server, and it becomes the page's own `unknown` - not `never`, which
   * would claim nothing has ever fetched on a machine that has.
   */
  it('narrows a check state outside the shipped set to unknown, carrying nothing', () => {
    const wire = modelsFrom(payload({ check: { state: 'stalled' } }));

    expect(wire.check).toEqual({ state: 'unknown' });
  });

  it('keeps the four known check states as the server wrote them', () => {
    // The check is a tagged enum, so even its bare states cross as an object
    // carrying the tag - `catalogue.rs`'s own test pins those names.
    expect(modelsFrom(payload({ check: { state: 'never' } })).check).toEqual({ state: 'never' });
    expect(modelsFrom(payload({ check: { state: 'checking' } })).check).toEqual({
      state: 'checking',
    });
    expect(modelsFrom(payload({ check: { state: 'unreachable', error: '502' } })).check).toEqual({
      state: 'unreachable',
      error: '502',
    });
    expect(modelsFrom(payload()).check).toEqual({
      state: 'fresh',
      at: '2026-10-06T06:12:00Z',
      release: 'v0.3.1',
      skipped: 2,
    });
  });

  /**
   * A role outside the two the core names is a model all the same, and a row
   * that dropped it would hide a pin. `other` is what the row's role cell
   * words, so the row is still drawn.
   */
  it('narrows a role outside the shipped set, in both the pins and the proposals', () => {
    const wire = modelsFrom(
      payload({
        in_use: [{ ...(payload().in_use[0] as InUseModel), role: 'summarizing' }],
        updates: [{ role: 'summarizing', file: 'x.gguf', current: {}, candidate: {} }],
      }),
    );

    expect(wire.in_use[0]?.role).toBe('other');
    expect(wire.updates[0]?.role).toBe('other');
  });

  /**
   * A model's own state is the same union the home's dictate card narrows, and
   * the same fallback the home chose: `pending` is drawn without claiming a
   * load the machine may not be doing.
   */
  it('narrows a model state outside the shipped set to pending', () => {
    const wire = modelsFrom(
      payload({ in_use: [{ ...(payload().in_use[0] as InUseModel), state: 'warming' }] }),
    );

    expect(wire.in_use[0]?.state).toBe('pending');
    const downloading = modelsFrom(
      payload({
        in_use: [
          {
            ...(payload().in_use[0] as InUseModel),
            state: { downloading: { downloaded: 10, total: 100, resumed_from: null } },
          },
        ],
      }),
    );
    expect(downloading.in_use[0]?.state).toEqual({
      downloading: { downloaded: 10, total: 100, resumed_from: null },
    });
  });

  /**
   * Everything else crosses as it stands: the pins' declared facts, the
   * proposal's numbers, and the feed's rows - the page's other three sections
   * draw them, so a narrowing that dropped one would empty a section.
   */
  it('carries the facts and the feed whole', () => {
    const row = {
      variant: 'granite-speech-5.0-470m-turboctc',
      display_name: 'Granite Speech 5.0 470M TurboCTC',
      family: 'granite',
      params: 470_000_000,
      license: 'Apache-2.0',
      languages: ['en'],
      streaming: false,
      download: { quant: 'Q4_K_M', size_bytes: 279_000_000 },
      speed: { machine: 'm4-max', backend: 'metal', quant: 'Q8_0', xrt_wall: 388.8 },
      wer: { dataset: 'fleurs', split: 'test', language: 'en', err_pct: 4.61 },
    };
    const wire = modelsFrom(
      payload({
        rows: [row],
        updates: [
          {
            role: 'transcribing',
            file: 'cohere-transcribe-03-2026-Q4_K_M.gguf',
            current: { speed_x: 72.9, fleurs_en_wer: 5.08 },
            candidate: row,
          },
        ],
      }),
    );

    expect(wire.enabled).toBe(true);
    expect(wire.models_dir).toBe('/tmp/models');
    expect(wire.in_use[0]?.facts).toEqual({
      quant: 'Q4_K_M',
      params: 2_049_026_832,
      license: 'Apache-2.0',
      runtime: 'transcribe.cpp',
    });
    expect(wire.rows).toEqual([row]);
    expect(wire.updates[0]?.current).toEqual({ speed_x: 72.9, fleurs_en_wer: 5.08 });
    expect(wire.updates[0]?.candidate.speed?.xrt_wall).toBe(388.8);
  });
});
