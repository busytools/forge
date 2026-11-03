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
        from: { from: 'pin' },
        at: null,
      },
      {
        role: 'normalization',
        file: 's1-mini-f16.gguf',
        size: 1_509_347_232,
        sha256: '0370da4f',
        state: 'loading',
        facts: { quant: 'F16', params: 596_000_000, license: 'Apache-2.0', runtime: 'llama.cpp' },
        catalogue: null,
        from: { from: 'pin' },
        at: null,
      },
    ],
    check: { state: 'fresh', at: '2026-10-06T06:12:00Z', release: 'v0.3.1', skipped: 2 },
    updates: [],
    rows: [],
    install: { state: 'idle' },
    activate: { state: 'idle' },
    installed: [],
    bench: { state: 'idle' },
    results: [],
    read_aloud: { recorded: false, recording: false, error: null, passage: '' },
    speaking: {
      enabled: true,
      models_dir: '/tmp/tts-models',
      speaking_model: 'hexgrad/Kokoro-82M',
      active: {
        variant: 'kokoro-82m-int8',
        file: 'model_quantized.onnx',
        quant: 'int8',
        size: 92_361_116,
        installed: true,
        active: true,
      },
      from: 'pin',
      variants: [],
      voices: ['af_heart'],
      voice: null,
    },
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

  /** An older server sends no speaking read: the page draws the role as
   * unread rather than as an installed model that is not there. */
  it('defaults a missing speaking read to unknown, with no directory', () => {
    const wire = modelsFrom(payload({ models_dir: undefined, speaking: undefined }));

    expect(wire.models_dir).toBeNull();
    expect(wire.speaking.from).toBe('unknown');
    expect(wire.speaking.active).toBeNull();
    expect(wire.speaking.voices).toEqual([]);
  });

  /** The speaking source is narrowed like every other union. */
  it('narrows an unknown speaking source to unknown', () => {
    const base = payload({}).speaking;
    const wire = modelsFrom(payload({ speaking: { ...base, from: 'again' } }));

    expect(wire.speaking.from).toBe('unknown');
  });

  /** The wire emits a bare name. A tagged `{"from":"installed"}` object is a
   * shape this client cannot read and must not be mistaken for a source,
   * which is the bug that hid the whole speaking UI. */
  it('does not read a tagged from object as a source', () => {
    const base = payload({}).speaking;
    const wire = modelsFrom(payload({ speaking: { ...base, from: { from: 'installed' } } }));

    expect(wire.speaking.from).toBe('unknown');
  });

  it('carries a preview render failure beside the speaking read', () => {
    const base = payload({}).speaking;
    const wire = modelsFrom(payload({ speaking: { ...base, preview_error: 'the render died' } }));

    expect(wire.speaking.preview_error).toBe('the render died');
  });

  /** The speaking bench's state is narrowed at the same boundary: a page
   * that trusted an unknown tag would draw a run that is not there. */
  it('narrows a speaking bench state outside the shipped set to unknown', () => {
    const wire = modelsFrom(payload({ speaking_bench: { state: 'paused' } }));

    expect(wire.speaking_bench).toEqual({ state: 'unknown' });
  });

  it('keeps a running speaking bench with its tick', () => {
    const wire = modelsFrom(payload({ speaking_bench: { state: 'running', clip: 3, clips: 12 } }));

    expect(wire.speaking_bench).toEqual({ state: 'running', clip: 3, clips: 12 });
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
        updates: [
          {
            role: 'summarizing',
            file: 'x.gguf',
            current: {},
            candidates: [
              {
                row: { kind: 'sideways', url: 7, download_count: 'many', params: null },
                verdict: 'sideways',
              },
            ],
          },
        ],
      }),
    );

    expect(wire.in_use[0]?.role).toBe('other');
    expect(wire.updates[0]?.role).toBe('other');
    expect(wire.updates[0]?.candidates[0]?.verdict).toBe('unknown');
    // A candidate's row narrows like the feed's own rows do: the fields a
    // renderer switches on are narrowed where they enter, whichever list
    // they crossed in.
    const row = wire.updates[0]?.candidates[0]?.row;
    expect(row?.kind).toBe('other');
    expect(row?.url).toBeNull();
    expect(row?.download_count).toBeNull();
    expect(row?.params, 'a Hub row counts no parameters, and null crosses as itself').toBeNull();
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
      kind: 'asr',
      url: null,
      download_count: null,
    };
    const wire = modelsFrom(
      payload({
        rows: [row],
        updates: [
          {
            role: 'transcribing',
            file: 'cohere-transcribe-03-2026-Q4_K_M.gguf',
            current: { speed_x: 72.9, fleurs_en_wer: 5.08 },
            candidates: [{ row, verdict: 'recommended' }],
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
    expect(wire.updates[0]?.candidates[0]?.row.speed?.xrt_wall).toBe(388.8);
  });

  /**
   * Where a role's model came from is the fourth union this page narrows. An
   * unreadable source keeps the row - the model is still in use - and says
   * only that this client cannot place it.
   */
  it('narrows an active-model source outside the shipped set, keeping its own', () => {
    const sourced = (from: unknown) =>
      modelsFrom(payload({ in_use: [{ ...(payload().in_use[0] as InUseModel), from }] })).in_use[0]
        ?.from;

    expect(sourced({ from: 'borrowed' })).toEqual({ from: 'unknown' });
    expect(sourced({ from: 'pin' })).toEqual({ from: 'pin' });
    expect(sourced({ from: 'installed', variant: 'granite' })).toEqual({
      from: 'installed',
      variant: 'granite',
    });
    expect(sourced({ from: 'config', key: 'transcribe_model', variant: 'granite' })).toEqual({
      from: 'config',
      key: 'transcribe_model',
      variant: 'granite',
    });
  });

  /**
   * A download state this client is older than says so rather than reading as
   * idle: `idle` here would claim nothing is downloading on a forge that is.
   */
  it('narrows a download state it cannot read rather than drawing idle', () => {
    expect(modelsFrom(payload({ install: { state: 'paused' } })).install).toEqual({
      state: 'unknown',
    });
    expect(
      modelsFrom(payload({ install: { state: 'downloading', file: 'x.gguf', got: 1, total: 2 } }))
        .install,
    ).toEqual({ state: 'downloading', file: 'x.gguf', got: 1, total: 2 });
  });

  /** The activation state carries a role, and it is narrowed the same way. */
  it('narrows an activation state and the role it names', () => {
    const activating = modelsFrom(
      payload({ activate: { state: 'activating', role: 'summarizing', file: 'x.gguf' } }),
    );

    expect(activating.activate).toEqual({ state: 'activating', role: 'other', file: 'x.gguf' });
    expect(modelsFrom(payload({ activate: { state: 'queued' } })).activate).toEqual({
      state: 'unknown',
    });
  });
});
