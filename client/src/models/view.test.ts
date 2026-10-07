import { describe, expect, it } from 'vitest';

// **The clock's claim is "the reader's own time, not the stamp's text", so
// this file pins a zone whose offset is not zero.** On a UTC runner - which
// is what CI is - a UTC-hardcoded body agrees with the conversion on every
// stamp, and the test would discriminate only on a developer's machine. The
// assignment is read by `Date` from the next construction on.
process.env.TZ = 'Asia/Kolkata';

import type {
  BenchResult,
  CatalogueRow,
  DictateModelsWire,
  InstalledModel,
  InUseModel,
} from '../wire/models';
import { modelsWire } from './testing';
import {
  activateLine,
  activeSource,
  candidateFacts,
  checkLine,
  clock,
  entryUrl,
  installLine,
  inUseLicense,
  inUseRowFacts,
  languagesLabel,
  modelChip,
  paramsLabel,
  roleWord,
  rowAction,
  search,
  sizeLabel,
  speedLabel,
  comparison,
  sweepHeadline,
  sweepPlan,
  sweepScope,
  sweepVerdicts,
} from './view';

/** A catalogue row with every fact the candidate list draws. */
function row(over: Partial<CatalogueRow> = {}): CatalogueRow {
  return {
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
    ...over,
  };
}

describe('the numbers the rows draw', () => {
  /**
   * Decimal units with the mock's own precision: a machine that read these as
   * binary (GiB) would draw 1.45 GB for the same file, and a reader comparing
   * the row against the catalogue it came from would find no such number.
   */
  it('sizes a download in decimal units, two decimals at the gigabyte', () => {
    expect(sizeLabel(1_558_162_944)).toBe('1.56 GB');
    expect(sizeLabel(1_509_347_232)).toBe('1.51 GB');
    expect(sizeLabel(279_000_000)).toBe('279 MB');
    expect(sizeLabel(477_000_000)).toBe('477 MB');
    // A recording is seconds long: megabytes would round it down to `0 MB`.
    expect(sizeLabel(64_044)).toBe('64 KB');
    expect(sizeLabel(1_004_800)).toBe('1 MB');
  });

  /**
   * A parameter count in the unit a reader scans for, not ten digits - and
   * the millions rounded, because the feed's own figure for the granite
   * candidate is 473,010,000 and `473.01M` is a precision nobody reads for.
   */
  it('names a parameter count by its unit', () => {
    expect(paramsLabel(2_049_026_832)).toBe('2.05B');
    expect(paramsLabel(596_000_000)).toBe('596M');
    expect(paramsLabel(473_010_000)).toBe('473M');
  });

  it('keeps one decimal on a speed, which is the number the feed measures', () => {
    expect(speedLabel(72.9)).toBe('72.9\u{d7}');
    expect(speedLabel(388.8)).toBe('388.8\u{d7}');
    expect(speedLabel(214)).toBe('214.0\u{d7}');
  });

  it('says what a language list means to the pin', () => {
    expect(languagesLabel(['en'])).toBe('English only');
    expect(languagesLabel(['en', 'fr'])).toBe('2 languages');
    expect(languagesLabel(['de', 'fr'])).toBe('2 languages');
    // Nothing measured draws nothing, here as everywhere: an empty list is
    // not the same claim as "unknown".
    expect(languagesLabel([])).toBeNull();
  });
});

describe('the marks and words the page draws', () => {
  /**
   * The role cell: the pin's own words, and `other` for a role this client is
   * older than - a row that dropped the model instead would hide a pin.
   */
  it('words a role, and keeps an unknown one as a row', () => {
    expect(roleWord('transcribing')).toBe('transcribing');
    expect(roleWord('normalization')).toBe('cleanup');
    expect(roleWord('other')).toBe('model');
  });

  /**
   * A model's chip: the state's own word with the mark that carries it by
   * shape. The load in flight is the ring, a failure is the cross, and ready
   * is the disc - never colour alone.
   */
  it('draws a model state as a word and a shape', () => {
    expect(modelChip('ready')).toEqual({ mark: 'ok', text: 'loaded' });
    expect(modelChip('pending')).toEqual({ mark: 'off', text: 'waiting' });
    expect(modelChip('loading')).toEqual({ mark: 'live', text: 'loading' });
    expect(modelChip({ failed: { other: { message: 'x' } } }).mark).toBe('failed');
  });

  /**
   * A download in flight carries how far it has got: the wire sends both
   * numbers, and a chip that dropped the fraction would leave a reader with
   * "fetching" for the minutes a 1.5 GB file takes.
   */
  it("carries a download's progress in its chip", () => {
    expect(modelChip({ downloading: { downloaded: 38, total: 100, resumed_from: null } })).toEqual({
      mark: 'live',
      text: 'fetching 38%',
    });
  });
});

describe('the catalogue check line', () => {
  /**
   * The four states the feed can be in, each with its own mark. A client that
   * drew `never` for a state it could not read would claim nothing had
   * fetched on a machine that had.
   */
  it('draws each check state as itself', () => {
    expect(checkLine({ state: 'checking' }, 0).mark).toBe('live');
    expect(checkLine({ state: 'never' }, 0).mark).toBe('off');
    expect(checkLine({ state: 'unreachable', error: 'github answered 502' }, 0)).toEqual(
      expect.objectContaining({ mark: 'failed', detail: 'github answered 502' }),
    );
    const unknown = checkLine({ state: 'unknown' }, 0);
    expect(unknown.mark).toBe('off');
    expect(unknown.title).toContain('this client');
  });

  /**
   * A fresh check with a proposal IS the update line; the same check with
   * nothing to propose is "up to date". A page that keyed only on freshness
   * would draw a healthy line while a better model sat in `updates`.
   */
  it('is an update line exactly when the feed proposes one', () => {
    const fresh = {
      state: 'fresh' as const,
      at: '2026-10-06T06:12:00Z',
      release: 'v0.3.1',
      skipped: 0,
    };
    expect(checkLine(fresh, 0)).toEqual(
      expect.objectContaining({ mark: 'ok', title: 'up to date' }),
    );
    expect(checkLine(fresh, 1)).toEqual(
      expect.objectContaining({ mark: 'warn', title: 'update available' }),
    );
  });

  /**
   * **An off forge's `never` is not "not yet".** The feed is read only while
   * `[dictate] enabled` is set, so the detail names the key that would fetch
   * it rather than promising a boot refresh that will not come.
   */
  it('says the feed is not read while the section is off', () => {
    expect(checkLine({ state: 'never' }, 0, false).detail).toContain('[dictate] enabled');
    expect(checkLine({ state: 'never' }, 0, true).detail).toContain('check');
  });

  /** The feed's own skipped count is a fact about the parse, and it is said. */
  it('says how many entries the feed carried that did not parse', () => {
    const line = checkLine(
      { state: 'fresh', at: '2026-10-06T06:12:00Z', release: null, skipped: 3 },
      0,
    );

    expect(line.detail).toContain('3');
  });
});

describe('the candidate facts', () => {
  /**
   * Every cell the candidate row draws comes off the row: the size of the
   * quant a machine would run, the speed and error the feed measured, and the
   * licence an adoption would pin. The speed is the highlighted part, which
   * is the number a reader compares down the list.
   */
  it('draws the quant, the numbers and the licence', () => {
    const facts = candidateFacts(row());

    expect(facts.spec).toEqual([
      { text: 'Q4_K_M 279 MB' },
      { text: '388.8\u{d7} realtime', hl: true },
      { text: 'FLEURS-en 4.61' },
      { text: 'Apache-2.0' },
    ]);
    expect(facts.kind).toEqual([{ text: 'English only' }, { text: 'offline' }]);
  });

  /** A row the feed has no numbers for draws no number, never a zero. */
  it('draws nothing the feed did not measure', () => {
    const facts = candidateFacts(
      row({ download: null, speed: null, wer: null, license: null, languages: [] }),
    );

    expect(facts.spec, 'no size, speed, error or licence is no part at all').toEqual([]);
    expect(facts.kind).toEqual([{ text: 'offline' }]);
  });
});

describe('the comparison table', () => {
  /**
   * The rule's whole claim is a comparison, so both sides of both numbers
   * are on the row: a table that named only the candidate's figures would
   * leave the reader to fetch the model in use's own row to judge it.
   */
  it("draws each candidate against the model in use, in the rule's order", () => {
    const rows = comparison({
      role: 'transcribing',
      file: 'cohere-transcribe-03-2026-Q4_K_M.gguf',
      current: { speed_x: 72.9, fleurs_en_wer: 5.08 },
      candidates: [
        {
          row: row({
            variant: 'faster-but-nc',
            license: 'CC-BY-NC-SA-4.0',
            speed: { machine: 'm4-max', backend: 'metal', quant: 'Q8_0', xrt_wall: 401.6 },
            wer: { dataset: 'fleurs', split: 'test', language: 'en', err_pct: 4.3 },
          }),
          verdict: 'recommended',
        },
        {
          row: row({
            variant: 'slower',
            speed: { machine: 'm4-max', backend: 'metal', quant: 'Q8_0', xrt_wall: 50.0 },
          }),
          verdict: 'slower',
        },
      ],
    });

    expect(rows).toHaveLength(2);
    expect(rows[0]?.speed).toBe('401.6\u{d7} vs 72.9\u{d7}');
    expect(rows[0]?.error).toBe('4.3% vs 5.08%');
    expect(rows[0]?.license, 'the licence is a column, not a filter').toBe('CC-BY-NC-SA-4.0');
    expect(rows[0]?.verdict).toBe('recommended');
    expect(rows[0]?.recommended).toBe(true);
    expect(rows[1]?.verdict).toBe('slower than this');
    expect(rows[1]?.recommended).toBe(false);
  });

  /** An entry the feed measured only partially draws only what it has. */
  it('says so rather than inventing a number the feed did not carry', () => {
    const rows = comparison({
      role: 'transcribing',
      file: 'x.gguf',
      current: { speed_x: 72.9, fleurs_en_wer: 5.08 },
      candidates: [{ row: row({ speed: null, wer: null }), verdict: 'blunter' }],
    });

    expect(rows[0]?.speed).toBeNull();
    expect(rows[0]?.error).toBeNull();
  });
});

describe('finding a model', () => {
  /**
   * A result opens somewhere real. The one place a catalogue entry can be
   * read by a person is the feed's own tree, and the row is a link to it:
   * a list of rows that go nowhere is what a reader tries first and finds
   * nothing behind.
   */
  it('points a row at its catalogue entry, wherever it is clicked from', () => {
    expect(entryUrl(row())).toBe(
      'https://github.com/handy-computer/transcribe.cpp/blob/main/catalog/granite-speech-5.0-470m-turboctc.json',
    );
    // The page serves whatever the feed names: a variant is an address
    // segment, so a name carrying a slash or a space is escaped rather than
    // silently pointing at another document.
    expect(entryUrl(row({ variant: 'a/b c' }))).toBe(
      'https://github.com/handy-computer/transcribe.cpp/blob/main/catalog/a%2Fb%20c.json',
    );
    // A Hub entry names its own page, which is where a reader reads it.
    expect(
      entryUrl(
        row({ kind: 'normalizer', url: 'https://huggingface.co/superwhisper/s1-mini-GGUF' }),
      ),
    ).toBe('https://huggingface.co/superwhisper/s1-mini-GGUF');
  });

  const rows = [
    row(),
    row({
      variant: 'parakeet-unified-en-0.6b',
      display_name: 'Parakeet Unified EN',
      family: 'parakeet',
    }),
    row({ variant: 'qwen3-asr-1.7b', display_name: 'Qwen3 ASR', family: 'qwen' }),
  ];

  it('matches the variant, the display name and the family, ignoring case', () => {
    expect(search(rows, 'PARAKEET').map((entry) => entry.variant)).toContain(
      'parakeet-unified-en-0.6b',
    );
    expect(search(rows, 'Qwen3').map((entry) => entry.variant)).toEqual(['qwen3-asr-1.7b']);
    expect(search(rows, 'granite').map((entry) => entry.variant)).toEqual([
      'granite-speech-5.0-470m-turboctc',
    ]);
  });

  it('finds nothing for a query nothing matches', () => {
    expect(search(rows, 'wav2vec')).toEqual([]);
  });
});

describe('the check clock', () => {
  /**
   * **The stamp is a time, not the text a slice would take from it.** The
   * server writes RFC 3339 in UTC and the page draws the reader's own clock.
   * The offset pair is the assertion that holds in every timezone: the same
   * instant written two ways reads the same, which `at.slice(11, 16)` cannot
   * do - it would print each stamp's own text - and the value equals what the
   * platform makes of the stamp.
   */
  it("reads the server's stamp as the local time of that instant", () => {
    const stamp = '2026-10-06T06:12:00Z';
    const rendered = new Date(stamp).toLocaleTimeString(undefined, {
      hour: '2-digit',
      minute: '2-digit',
      hour12: false,
    });

    expect(clock(stamp)).toBe(rendered);
    expect(
      clock('2026-10-06T11:42:00+05:30'),
      'the same instant at another offset read differently',
    ).toBe(rendered);
    expect(clock('whenever')).toBeNull();
  });
});

/** One in-use row, for the second meta line. */
function inUse(over: Partial<InUseModel> = {}): InUseModel {
  return {
    role: 'transcribing',
    file: 'cohere-transcribe-03-2026-Q4_K_M.gguf',
    size: 1_558_162_944,
    sha256: '0ea56826d8bd5d74',
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
      languages: ['en'],
      speed: { machine: 'm4-max', backend: 'metal', quant: 'Q8_0', xrt_wall: 72.9 },
    },
    from: { from: 'pin' },
    at: null,
    ...over,
  };
}

describe('the in-use rows', () => {
  it("states the pinned facts and the feed's measurement", () => {
    expect(inUseRowFacts(inUse())).toEqual({
      pinned: [
        { text: '1.56 GB', hl: true },
        { text: 'Q4_K_M' },
        { text: '2.05B params' },
        { text: 'sha 0ea56826' },
        { text: 'Apache-2.0' },
      ],
      measured: [
        { text: 'metal' },
        { text: '72.9\u{d7} realtime on m4-max', hl: true },
        { text: 'English only' },
      ],
    });
  });

  /**
   * **A model with no published digest draws no digest.** Every pin carries
   * one; a model downloaded from the feed's docs has none, and a row that
   * printed a placeholder would be claiming bytes nobody checked.
   */
  it('draws no digest for a model that has none', () => {
    const facts = inUseRowFacts(inUse({ sha256: null }));

    expect(facts.pinned.some((part) => part.text.startsWith('sha '))).toBe(false);
    expect(facts.pinned[0]).toEqual({ text: '1.56 GB', hl: true });
  });

  /**
   * A pin the feed does not carry still draws its own facts: the file, its
   * quant and its digest are the pin's, and a row that hid them because the
   * feed had no entry would hide the model in use. The runtime stands where
   * the measurement would be.
   */
  it('draws a pin the feed has no entry for', () => {
    const facts = inUseRowFacts(inUse({ catalogue: null }));

    expect(facts.pinned[0]).toEqual({ text: '1.56 GB', hl: true });
    expect(facts.measured).toEqual([{ text: 'transcribe.cpp' }, { text: 'not in the feed' }]);
  });

  /**
   * The comparison table's baseline row is the in-use model, and it draws a
   * licence in the licence column like every other row; with no row for the
   * role it says so rather than leaving the cell empty.
   */
  it('carries the in-use licence for the table', () => {
    expect(inUseLicense([inUse()], 'transcribing')).toBe('Apache-2.0');
    expect(inUseLicense([], 'transcribing')).toBe('no licence on the feed');
  });
});

describe("where a role's model came from", () => {
  /** The config pin names its key, because that key is what has to go. */
  it('names the [dictate] key a config pin holds the role with', () => {
    expect(activeSource({ from: 'config', key: 'transcribe_model', variant: 'granite' })).toBe(
      'pinned by [dictate] transcribe_model',
    );
  });

  it('says the compiled default for a pin, and nothing for a source it cannot read', () => {
    expect(activeSource({ from: 'pin' })).toBe('compiled default');
    expect(activeSource({ from: 'installed', variant: 'granite' })).toBe('installed here');
    expect(activeSource({ from: 'unknown' })).toBe('set by a newer forge');
  });
});

/** One installed record, for the row controls. */
function installed(over: Partial<InstalledModel> = {}): InstalledModel {
  return {
    variant: 'granite-speech-5.0-470m-turboctc',
    file: 'granite-speech-5.0-470m-turboctc-Q4_K_M.gguf',
    url: 'https://huggingface.co/handy-computer/granite-gguf/resolve/main/x.gguf',
    size: 279_000_000,
    facts: { quant: 'Q4_K_M', params: 470_000_000, license: 'Apache-2.0', runtime: null },
    at: '2026-10-06T09:00:00Z',
    ...over,
  };
}

describe('what a catalogue row offers', () => {
  /** A model not on this machine: the control is the download. */
  it('offers the download for a variant this machine does not have', () => {
    expect(rowAction(row(), [], [inUse()])).toEqual({ do: 'install', label: 'install Q4_K_M' });
  });

  /** Installed and not active: the control is the activation, naming its file. */
  it('offers the activation once the variant is installed', () => {
    expect(rowAction(row(), [installed()], [inUse()])).toEqual({
      do: 'activate',
      label: 'use for transcribing',
      file: 'granite-speech-5.0-470m-turboctc-Q4_K_M.gguf',
      role: 'transcribing',
    });
  });

  /** The active model is a state, not an action. */
  it('draws the active model as a state rather than a control', () => {
    const active = inUse({ file: 'granite-speech-5.0-470m-turboctc-Q4_K_M.gguf' });

    expect(rowAction(row(), [installed()], [active])).toEqual({ do: 'off', label: 'active' });
  });

  /**
   * **A pinned role draws no activation control.** `forge.toml` wins over
   * every runtime pick, so the core refuses the dispatch - and a control
   * that is always refused reads as broken. The download stays.
   */
  it('draws no activation control while [dictate] pins the role', () => {
    const pinned = inUse({
      role: 'transcribing',
      from: { from: 'config', key: 'transcribe_model', variant: 'cohere-transcribe-03-2026' },
    });

    expect(rowAction(row(), [installed()], [pinned])).toEqual({ do: 'off', label: 'installed' });
    expect(rowAction(row({ variant: 'other' }), [], [pinned])).toEqual({
      do: 'install',
      label: 'install Q4_K_M',
    });
  });

  /** An entry with no download this machine would run offers nothing. */
  it('offers nothing for an entry with no download', () => {
    expect(rowAction(row({ download: null }), [], [inUse()])).toEqual({ do: 'none' });
  });
});

describe('the operation lines', () => {
  it('draws the download with its whole-percent figure', () => {
    expect(
      installLine({
        state: 'downloading',
        file: 'granite-Q4_K_M.gguf',
        got: 50_000_000,
        total: 100_000_000,
      }),
    ).toEqual({
      mark: 'live',
      title: 'downloading granite-Q4_K_M.gguf',
      detail: '50% \u{b7} 50 MB of 100 MB',
      percent: 50,
    });
  });

  it("draws a failed download with the core's own reason", () => {
    const line = installLine({
      state: 'failed',
      file: 'granite-Q4_K_M.gguf',
      reason: 'granite-Q4_K_M.gguf is 5 bytes, expected 6',
    });

    expect(line?.mark).toBe('failed');
    expect(line?.detail).toContain('is 5 bytes, expected 6');
    expect(line?.percent).toBeNull();
  });

  it("draws nothing while idle, and an unreadable state as this client's own", () => {
    expect(installLine({ state: 'idle' })).toBeNull();
    expect(installLine({ state: 'unknown' })?.mark).toBe('off');
    expect(activateLine({ state: 'idle' })).toBeNull();
    expect(activateLine({ state: 'unknown' })?.mark).toBe('off');
  });

  /**
   * The window between the press and the feed's doc answering: no name and
   * no size yet. `0 of 0` here reads as a stalled download; the line says
   * what is actually happening instead.
   */
  it('draws the unnamed window before the file is known', () => {
    const reading = installLine({ state: 'downloading', file: '', got: 0, total: 0 });
    expect(reading?.title).toBe('reading the catalogue entry');
    expect(reading?.detail).toBeNull();
    expect(reading?.percent).toBeNull();

    const named = installLine({ state: 'downloading', file: 'granite.gguf', got: 0, total: 0 });
    expect(named?.title).toBe('downloading granite.gguf');
    expect(named?.detail).toBeNull();
  });

  /** A failed activation says the current model is still running. */
  it('draws a failed activation with the model that keeps running', () => {
    const line = activateLine({
      state: 'failed',
      role: 'transcribing',
      file: 'granite-Q4_K_M.gguf',
      reason: 'the file is not a model',
    });

    expect(line?.detail).toContain('the file is not a model');
    expect(line?.detail).toContain('the current model is still running');
  });
});

describe('the sweep', () => {
  /** One cleanup candidate as the feed lists it: popularity and a size. */
  function normalizer(variant: string, downloads: number, size = 300_000_000): CatalogueRow {
    return row({
      variant,
      display_name: variant,
      kind: 'normalizer',
      download_count: downloads,
      download: { quant: 'Q4_K_M', size_bytes: size },
    });
  }

  function benchResult(
    file: string,
    role: 'transcribing' | 'cleanup',
    corpus: string,
    metrics: Partial<BenchResult['metrics']> = {},
  ): BenchResult {
    return {
      target: { file, role, pinned: false },
      tier: 'consensus',
      metrics: {
        clips: 12,
        audio_seconds: 320,
        wall_seconds: 210,
        xrt_wall: 30,
        term_accuracy: null,
        wer: null,
        matched: null,
        stages_ms: {
          model_load_ms: 1200,
          resample_ms: 10,
          mel_ms: 20,
          encode_ms: 30,
          decode_ms: 40,
          normalize_ms: 50,
        },
        ...metrics,
      },
      at: '2026-10-07T10:00:00Z',
      corpus: { clips: 12, audio_seconds: 320, sha256: corpus },
    };
  }

  /**
   * The plan a press draws: the feed's own transcribing pick, the cleanup
   * role's most-downloaded candidates, and the in-use cleanup model read
   * against - priced before anything moves.
   */
  it('plans the runs by downloads and prices only what must come down', () => {
    const wire: DictateModelsWire = {
      ...modelsWire,
      rows: [
        normalizer('a/norm-b', 500),
        normalizer('a/norm-a', 900),
        normalizer('a/norm-c', 300),
        normalizer('a/norm-d', 100),
      ],
    };

    const plan = sweepPlan(wire);

    expect(plan.runs.map((run) => run.why)).toEqual([
      'pick',
      'baseline',
      'baseline',
      'candidate',
      'candidate',
      'candidate',
    ]);
    expect(plan.runs.map((run) => run.variant)).toEqual([
      'granite-speech-5.0-470m-turboctc-nc',
      'cohere-transcribe-03-2026',
      's1-mini-f16.gguf',
      'a/norm-a',
      'a/norm-b',
      'a/norm-c',
    ]);
    // The pick is on this machine already, and the baseline is what runs:
    // the three candidates are the whole download.
    expect(plan.bytes).toBe(900_000_000);
    expect(plan.beyond).toBe(1);
    expect(plan.tier).toBe('consensus');
    expect(plan.seconds_runs).toBeNull();
  });

  /**
   * The baseline is the file the role RUNS. A pin can run one quant while
   * the store holds another under the same variant, and reading the store's
   * file would bench a model that is not the one being compared against.
   */
  it('takes the baseline from what runs, and never benches it twice', () => {
    const wire: DictateModelsWire = {
      ...modelsWire,
      in_use: modelsWire.in_use.map((model) =>
        model.role === 'normalization'
          ? {
              ...model,
              catalogue: {
                variant: 'a/norm-a',
                display_name: 'Norm',
                size_bytes: 1,
                streaming: false,
                languages: ['en'],
                speed: null,
              },
            }
          : model,
      ),
      installed: [
        ...modelsWire.installed,
        {
          variant: 'a/norm-a',
          file: 'a-norm-a-Q4_K_M.gguf',
          url: 'https://huggingface.co/a/norm-a',
          size: 300_000_000,
          facts: { quant: 'Q4_K_M', params: 600_000_000, license: null, runtime: 'llama.cpp' },
          at: '2026-10-07T09:00:00Z',
        },
      ],
      rows: [normalizer('a/norm-a', 900), normalizer('a/norm-b', 500)],
    };

    const plan = sweepPlan(wire);
    const baseline = plan.runs.find((run) => run.why === 'baseline' && run.role === 'cleanup');
    const candidates = plan.runs.filter((run) => run.why === 'candidate');

    expect(baseline?.file).toBe('s1-mini-f16.gguf');
    expect(baseline?.installed).toBe(true);
    // The candidate the role already runs is not a second run.
    expect(candidates.map((run) => run.variant)).toEqual(['a/norm-b']);
    expect(plan.bytes).toBe(300_000_000);
  });

  /** The read-aloud set, once recorded, is the corpus a press scores on. */
  it('scores on the read-aloud set when one is recorded', () => {
    const wire: DictateModelsWire = {
      ...modelsWire,
      read_aloud: {
        ...modelsWire.read_aloud,
        recordings: [
          {
            id: 'take-1700000000000',
            duration_ms: 14_000,
            bytes: 64_044,
            sha256: 'ab'.repeat(32),
            at: '2026-10-07T09:00:00Z',
          },
        ],
      },
    };

    expect(sweepPlan(wire).tier).toBe('read_aloud');
  });

  /**
   * One corpus is one comparison: the verdict reads the newest corpus the
   * plan's runs share, and a better number from another corpus is not a
   * number about this one.
   */
  it('reads the verdict off one corpus, against the model in use', () => {
    const wire: DictateModelsWire = {
      ...modelsWire,
      rows: [normalizer('a/norm-a', 900)],
      installed: [
        ...modelsWire.installed,
        {
          variant: 'a/norm-a',
          file: 'a-norm-a-Q4_K_M.gguf',
          url: 'https://huggingface.co/a/norm-a',
          size: 300_000_000,
          facts: { quant: 'Q4_K_M', params: 600_000_000, license: null, runtime: 'llama.cpp' },
          at: '2026-10-07T09:00:00Z',
        },
      ],
    };
    const plan = sweepPlan(wire);
    wire.results = [
      benchResult('a-norm-a-Q4_K_M.gguf', 'cleanup', 'new', { wer: 0.08, matched: [9, 12] }),
      benchResult('s1-mini-f16.gguf', 'cleanup', 'new', { wer: 0.12, matched: [7, 12] }),
      // A better run from before the corpus moved: it must not win.
      benchResult('a-norm-a-Q4_K_M.gguf', 'cleanup', 'old', { wer: 0.01, matched: [12, 12] }),
      benchResult('granite-speech-5.0-470m-turboctc-nc-Q4_K_M.gguf', 'transcribing', 'new', {
        wer: 0.06,
      }),
    ];

    const cleanup = sweepVerdicts(wire, plan).find((verdict) => verdict.role === 'cleanup');
    if (cleanup === undefined) throw new Error('the cleanup verdict did not form');

    expect(cleanup.best.result.target.file).toBe('a-norm-a-Q4_K_M.gguf');
    expect(cleanup.baseline?.target.file).toBe('s1-mini-f16.gguf');
    expect(cleanup.onBest).toBe(false);
    expect(cleanup.scored).toBe(2);
    expect(cleanup.tried).toBe(1);
    expect(cleanup.beyond).toBe(0);
    expect(cleanup.tier).toBe('consensus');

    // The verdict may never say a bare best: the scope carries what it saw.
    expect(sweepScope(cleanup)).toContain('the most-downloaded cleanup candidate');
    expect(sweepScope(cleanup)).toContain('your takes, 12 clips');
    expect(sweepHeadline(cleanup)).toContain('a-norm-a-Q4_K_M.gguf read better than');
  });

  /**
   * A sweep with nothing proposed scores the model in use alone, and its
   * scope says that rather than claiming a pick the plan never carried.
   */
  it('says when the feed proposed nothing to score against', () => {
    const wire: DictateModelsWire = { ...modelsWire, updates: [], rows: [] };
    const plan = sweepPlan(wire);
    wire.results = [
      benchResult('cohere-transcribe-03-2026-Q4_K_M.gguf', 'transcribing', 'now', { wer: 0.09 }),
    ];

    const transcribing = sweepVerdicts(wire, plan).find(
      (verdict) => verdict.role === 'transcribing',
    );
    if (transcribing === undefined) throw new Error('the transcribing verdict did not form');

    expect(transcribing.pick).toBe(false);
    expect(transcribing.scored).toBe(1);
    expect(sweepScope(transcribing)).toContain('only the model you run');
    expect(sweepScope(transcribing)).toContain('nothing else was proposed');
    expect(sweepHeadline(transcribing)).toBe(
      'the transcribing model you run read best of the 1 scored',
    );
  });

  /** With a pick in the plan, the scope names it - the other half of the rule. */
  it('names the pick in the scope when one was proposed', () => {
    const wire: DictateModelsWire = { ...modelsWire, rows: [] };
    const plan = sweepPlan(wire);
    wire.results = [
      benchResult('granite-speech-5.0-470m-turboctc-nc-Q4_K_M.gguf', 'transcribing', 'now', {
        wer: 0.06,
      }),
    ];

    const transcribing = sweepVerdicts(wire, plan).find(
      (verdict) => verdict.role === 'transcribing',
    );
    if (transcribing === undefined) throw new Error('the transcribing verdict did not form');

    expect(transcribing.pick).toBe(true);
    expect(sweepScope(transcribing)).toContain("the feed's own pick");
  });

  /** On the best is its own verdict, and it says so. */
  it('says when what runs is the best of what was scored', () => {
    const wire: DictateModelsWire = {
      ...modelsWire,
      rows: [normalizer('a/norm-a', 900)],
      installed: [
        ...modelsWire.installed,
        {
          variant: 'a/norm-a',
          file: 'a-norm-a-Q4_K_M.gguf',
          url: 'https://huggingface.co/a/norm-a',
          size: 300_000_000,
          facts: { quant: 'Q4_K_M', params: 600_000_000, license: null, runtime: 'llama.cpp' },
          at: '2026-10-07T09:00:00Z',
        },
      ],
    };
    const plan = sweepPlan(wire);
    wire.results = [
      benchResult('a-norm-a-Q4_K_M.gguf', 'cleanup', 'new', { wer: 0.14 }),
      benchResult('s1-mini-f16.gguf', 'cleanup', 'new', { wer: 0.11 }),
    ];

    const cleanup = sweepVerdicts(wire, plan).find((verdict) => verdict.role === 'cleanup');
    if (cleanup === undefined) throw new Error('the cleanup verdict did not form');

    expect(cleanup.onBest).toBe(true);
    expect(sweepHeadline(cleanup)).toBe('the cleanup model you run read best of the 2 scored');
  });
});
