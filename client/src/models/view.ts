/**
 * The models page's reads gathered into the shape its markup wants: the
 * numbers the rows print, the mark each state carries, and the check line.
 *
 * Pure, so a test builds a feed by hand. Everything here comes off the
 * snapshot; nothing is recomputed from anything else the client holds.
 */

import { modelState } from '../home/view';
import type {
  CatalogueCheck,
  CatalogueRow,
  InUseModel,
  ModelRole,
  ModelUpdate,
} from '../wire/models';
import type { DictateModelState } from '../wire/types';

/** A download size in the units the catalogue quotes its own figures in. */
export function sizeLabel(bytes: number): string {
  if (bytes >= 1_000_000_000) return `${(bytes / 1_000_000_000).toFixed(2)} GB`;
  return `${Math.round(bytes / 1_000_000)} MB`;
}

/**
 * A parameter count by its unit, which is how the model names spell it.
 *
 * Millions round to a whole number: the feed's figures are counts, not
 * measurements, and `473.01M` is a precision nobody reads for.
 */
export function paramsLabel(params: number): string {
  if (params >= 1_000_000_000) return `${trim(params / 1_000_000_000)}B`;
  return `${Math.round(params / 1_000_000)}M`;
}

/** A number with at most two decimals and no trailing zeros. */
function trim(value: number): string {
  return value.toFixed(2).replace(/\.?0+$/, '');
}

/** A speed: the wall-clock realtime factor the feed measures, which is an `x`. */
export function speedLabel(x: number): string {
  return `${x.toFixed(1)}\u{d7}`;
}

/** What a language list means, as the two rows have room for. */
export function languagesLabel(languages: string[]): string | null {
  const [first] = languages;
  if (first === undefined) return null;
  if (languages.length === 1) return first === 'en' ? 'English only' : first;
  return `${languages.length} languages`;
}

/** The role cell's word. */
export function roleWord(role: ModelRole): string {
  switch (role) {
    case 'transcribing':
      return 'transcribing';
    case 'normalization':
      return 'cleanup';
    // A role the core added after this client: the row is still a model, and
    // saying so keeps it drawn rather than hidden.
    case 'other':
      return 'model';
  }
}

/**
 * A model's live state as the chip draws it: the word the home's dictate card
 * already uses for the same state, and the shape that carries it without
 * colour.
 */
export function modelChip(state: DictateModelState): { mark: string; text: string } {
  const text = modelState(state);
  if (typeof state === 'object' && 'downloading' in state) {
    const { downloaded, total } = state.downloading;
    const percent = total > 0 ? ` ${Math.floor((downloaded / total) * 100)}%` : '';
    return { mark: 'live', text: `${text}${percent}` };
  }
  return { mark: markForState(state), text };
}

function markForState(state: DictateModelState): string {
  if (typeof state === 'object') return 'failed';
  switch (state) {
    case 'ready':
      return 'ok';
    case 'pending':
      return 'off';
    case 'verifying':
    case 'fetched':
    case 'loading':
      return 'live';
  }
}

/** The check line: the feed's freshness, and what it proposes. */
export interface CheckLine {
  mark: string;
  title: string;
  when: string | null;
  detail: string | null;
}

/**
 * The line the UPDATES section draws, from the check and the proposals it
 * carried.
 *
 * A fresh check is an update line exactly when the feed proposes something:
 * the two are one state of the same thing, and splitting them would draw a
 * healthy `up to date` beside a named candidate.
 */
export function checkLine(check: CatalogueCheck, updates: number, enabled = true): CheckLine {
  switch (check.state) {
    case 'never':
      // The feed is read only while the section is on, so an off forge's
      // `never` is not "not yet": nothing will fetch until the key is set.
      return {
        mark: 'off',
        title: 'not checked yet',
        when: null,
        detail: enabled
          ? 'the feed refreshes at the next boot, or when you check it'
          : 'the feed is read only with [dictate] enabled set',
      };
    case 'checking':
      return { mark: 'live', title: 'checking the catalogue', when: null, detail: null };
    case 'fresh': {
      const at = clock(check.at);
      const release = check.release === null ? '' : ` \u{b7} catalogue ${check.release}`;
      const skipped =
        check.skipped > 0
          ? `${check.skipped} ${check.skipped === 1 ? 'entry' : 'entries'} in the feed did not parse`
          : null;
      return {
        mark: updates > 0 ? 'warn' : 'ok',
        title: updates > 0 ? 'update available' : 'up to date',
        when: at === null ? null : `checked ${at}${release}`,
        detail: skipped,
      };
    }
    case 'unreachable':
      return {
        mark: 'failed',
        title: 'the catalogue could not be reached',
        when: null,
        detail: check.error,
      };
    case 'unknown':
      return {
        mark: 'off',
        title: 'the check state is one this client cannot read',
        when: null,
        detail: 'this client is older than the forge serving it',
      };
  }
}

/** The server's RFC 3339 stamp as a local time, or `null` when it is not one. */
export function clock(at: string): string | null {
  const when = new Date(at);
  if (Number.isNaN(when.getTime())) return null;
  return when.toLocaleTimeString(undefined, { hour: '2-digit', minute: '2-digit', hour12: false });
}

/**
 * One part of a facts line: the text, and whether it is the part the line
 * highlights - the number a reader compares down the list.
 */
export interface FactPart {
  text: string;
  hl?: boolean;
}

/** The face of one catalogue row, as the candidate list draws it. */
export interface CandidateFacts {
  /** The quant a machine would run, the speed, the error and the licence. */
  spec: FactPart[];
  /** What it transcribes, and whether a take can stream through it. */
  kind: FactPart[];
}

/**
 * Every fact the candidate row draws, from the row itself.
 *
 * A fact the feed did not carry is no part at all, never a stand-in: a row
 * that printed a zero speed or an empty licence would be read as a
 * measurement.
 */
export function candidateFacts(row: CatalogueRow): CandidateFacts {
  const spec: FactPart[] = [];
  if (row.download !== null) {
    spec.push({ text: `${row.download.quant} ${sizeLabel(row.download.size_bytes)}` });
  }
  if (row.speed !== null)
    spec.push({ text: `${speedLabel(row.speed.xrt_wall)} realtime`, hl: true });
  if (row.wer !== null) {
    spec.push({ text: `${row.wer.dataset.toUpperCase()}-${row.wer.language} ${row.wer.err_pct}` });
  }
  if (row.license !== null) spec.push({ text: row.license });

  const kind: FactPart[] = [];
  const languages = languagesLabel(row.languages);
  if (languages !== null) kind.push({ text: languages });
  kind.push({ text: row.streaming ? 'streaming' : 'offline' });

  return { spec, kind };
}

/**
 * The line an update draws: the candidate's own facts, each number the
 * comparison it was admitted on.
 */
export function updateFacts(update: ModelUpdate): FactPart[] {
  const candidate = update.candidate;
  const parts: FactPart[] = [{ text: paramsLabel(candidate.params) }];

  if (candidate.download !== null) {
    parts.push({ text: `${candidate.download.quant} ${sizeLabel(candidate.download.size_bytes)}` });
  }
  if (candidate.speed !== null) {
    parts.push({
      text: `${speedLabel(candidate.speed.xrt_wall)} vs ${speedLabel(update.current.speed_x)}`,
      hl: true,
    });
  }
  if (candidate.wer !== null) {
    parts.push({
      text: `${candidate.wer.dataset.toUpperCase()}-${candidate.wer.language} ${candidate.wer.err_pct} vs ${update.current.fleurs_en_wer}`,
    });
  }
  if (candidate.license !== null) parts.push({ text: candidate.license });

  const languages = languagesLabel(candidate.languages);
  parts.push({
    text: [languages, candidate.streaming ? 'streaming' : 'offline']
      .filter((word) => word !== null)
      .join(', '),
  });

  return parts;
}

/**
 * Where one catalogue entry can be read.
 *
 * The feed's own tree, which is where the server fetches the same documents
 * from (`forge-dictate`'s `CatalogueSource`); this is the page a person can
 * open to see one. The client asks for no read for it, and the link is the
 * entry's own document rather than a search.
 */
export function entryUrl(variant: string): string {
  return `https://github.com/handy-computer/transcribe.cpp/blob/main/catalog/${encodeURIComponent(variant)}.json`;
}

/** Find a model: the whole feed is already here, so the search is this side's. */
export function search(rows: CatalogueRow[], query: string): CatalogueRow[] {
  const needle = query.trim().toLowerCase();
  if (needle === '') return [];
  return rows.filter((row) =>
    [row.variant, row.display_name, row.family].some((field) =>
      field.toLowerCase().includes(needle),
    ),
  );
}

/** An in-use row's two fact lines. */
export interface InUseFacts {
  pinned: FactPart[];
  measured: FactPart[];
}

/**
 * The pinned facts, and what the feed says about the file.
 *
 * The pin's own declaration is what the first line states - size, quant,
 * parameters, digest, licence - because that is what forge verifies before a
 * load. The second is the feed's: how the runtime measured the file, and what
 * it transcribes. A file the feed does not carry still draws the pin, with
 * the runtime the pin declares standing where the measurement would be.
 */
export function inUseRowFacts(model: InUseModel): InUseFacts {
  const pinned: FactPart[] = [{ text: sizeLabel(model.size), hl: true }];
  if (model.facts.quant !== null) pinned.push({ text: model.facts.quant });
  if (model.facts.params !== null) {
    pinned.push({ text: `${paramsLabel(model.facts.params)} params` });
  }
  pinned.push({ text: `sha ${model.sha256.slice(0, 8)}` });
  if (model.facts.license !== null) pinned.push({ text: model.facts.license });

  const join = model.catalogue;
  if (join === null) {
    const runtime: FactPart[] = [];
    if (model.facts.runtime !== null) runtime.push({ text: model.facts.runtime });
    runtime.push({ text: 'not in the feed' });
    return { pinned, measured: runtime };
  }

  const measured: FactPart[] = [];
  if (join.speed !== null) {
    measured.push({ text: join.speed.backend });
    measured.push({
      text: `${speedLabel(join.speed.xrt_wall)} realtime on ${join.speed.machine}`,
      hl: true,
    });
  }
  const languages = languagesLabel(join.languages);
  if (languages !== null) measured.push({ text: languages });

  return { pinned, measured };
}
