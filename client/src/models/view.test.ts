import { describe, expect, it } from 'vitest';

// **The clock's claim is "the reader's own time, not the stamp's text", so
// this file pins a zone whose offset is not zero.** On a UTC runner - which
// is what CI is - a UTC-hardcoded body agrees with the conversion on every
// stamp, and the test would discriminate only on a developer's machine. The
// assignment is read by `Date` from the next construction on.
process.env.TZ = 'Asia/Kolkata';

import type { CatalogueRow, InUseModel } from '../wire/models';
import {
  candidateFacts,
  checkLine,
  clock,
  entryUrl,
  inUseRowFacts,
  languagesLabel,
  modelChip,
  paramsLabel,
  roleWord,
  search,
  sizeLabel,
  speedLabel,
  updateFacts,
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

describe('the update line', () => {
  /**
   * The proposal's whole claim is a comparison, so both sides of both numbers
   * are drawn: a line that named only the candidate's figures would leave the
   * reader to fetch the model in use's own row to judge it.
   */
  it('draws the candidate against the model in use', () => {
    const facts = updateFacts({
      role: 'transcribing',
      file: 'cohere-transcribe-03-2026-Q4_K_M.gguf',
      current: { speed_x: 72.9, fleurs_en_wer: 5.08 },
      candidate: row(),
    });

    expect(facts).toEqual([
      { text: '470M' },
      { text: 'Q4_K_M 279 MB' },
      { text: '388.8\u{d7} vs 72.9\u{d7}', hl: true },
      { text: 'FLEURS-en 4.61 vs 5.08' },
      { text: 'Apache-2.0' },
      { text: 'English only, offline' },
    ]);
  });

  /** A candidate the feed measured only partially draws only what it has. */
  it('draws no comparison the candidate has no number for', () => {
    const facts = updateFacts({
      role: 'transcribing',
      file: 'x.gguf',
      current: { speed_x: 72.9, fleurs_en_wer: 5.08 },
      candidate: row({ speed: null, wer: null, download: null, license: null, languages: [] }),
    });

    expect(facts).toEqual([{ text: '470M' }, { text: 'offline' }]);
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
    expect(entryUrl('granite-speech-5.0-470m-turboctc')).toBe(
      'https://github.com/handy-computer/transcribe.cpp/blob/main/catalog/granite-speech-5.0-470m-turboctc.json',
    );
    // The page serves whatever the feed names: a variant is an address
    // segment, so a name carrying a slash or a space is escaped rather than
    // silently pointing at another document.
    expect(entryUrl('a/b c')).toBe(
      'https://github.com/handy-computer/transcribe.cpp/blob/main/catalog/a%2Fb%20c.json',
    );
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
});
