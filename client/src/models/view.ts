/**
 * The models page's reads gathered into the shape its markup wants: the
 * numbers the rows print, the mark each state carries, and the check line.
 *
 * Pure, so a test builds a feed by hand. Everything here comes off the
 * snapshot; nothing is recomputed from anything else the client holds.
 */

import { modelState } from '../home/view';
import type {
  ActivateState,
  ActiveFrom,
  CatalogueCheck,
  CatalogueRow,
  InstallState,
  InstalledModel,
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

/**
 * Where an in-use row's model came from, in the row's own words. A config
 * pin names its key, because that key is what has to go for the runtime to
 * move the role.
 */
export function activeSource(from: ActiveFrom): string {
  switch (from.from) {
    case 'config':
      return `pinned by [dictate] ${from.key}`;
    case 'installed':
      return 'installed here';
    case 'pin':
      return 'compiled default';
    case 'unknown':
      return 'set by a newer forge';
  }
}

/**
 * What one catalogue row offers, from the page's own state.
 *
 * The order is the rule the core enforces: an installed model can be made
 * active, except while `forge.toml` pins the role - a pin cannot be moved at
 * runtime, so the row says `installed` rather than drawing a control that
 * would be refused. A feed entry with no download this machine would run
 * offers nothing.
 */
export type RowAction =
  | { do: 'install'; label: string }
  | { do: 'activate'; label: string; file: string }
  | { do: 'off'; label: string }
  | { do: 'none' };

export function rowAction(
  row: CatalogueRow,
  installed: InstalledModel[],
  transcribing: InUseModel | null,
): RowAction {
  const record = installed.find((model) => model.variant === row.variant);
  if (record !== undefined) {
    if (transcribing?.file === record.file) return { do: 'off', label: 'active' };
    if (transcribing?.from.from === 'config') return { do: 'off', label: 'installed' };
    return { do: 'activate', label: 'use for transcribing', file: record.file };
  }
  if (row.download === null) return { do: 'none' };
  return { do: 'install', label: `install ${row.download.quant}` };
}

/** One operation line: the download's or the activation's, drawn where the page's state is. */
export interface OpLine {
  mark: string;
  title: string;
  detail: string | null;
  /** The whole-percent figure, when the work has one to draw. */
  percent: number | null;
}

/** The download's line, or `null` when nothing is downloading. */
export function installLine(install: InstallState): OpLine | null {
  switch (install.state) {
    case 'idle':
      return null;
    case 'downloading': {
      // Before the file is named - the moment between the press and the
      // feed's doc answering - there is no size to draw and nothing honest
      // to say about bytes; `0 of 0` would read as a stalled download.
      if (install.total === 0) {
        return {
          mark: 'live',
          title:
            install.file === '' ? 'reading the catalogue entry' : `downloading ${install.file}`,
          detail: null,
          percent: null,
        };
      }
      const percent = Math.floor((install.got / install.total) * 100);
      return {
        mark: 'live',
        title: `downloading ${install.file}`,
        detail: `${percent}% \u{b7} ${sizeLabel(install.got)} of ${sizeLabel(install.total)}`,
        percent,
      };
    }
    case 'failed':
      return {
        mark: 'failed',
        title: 'the download did not finish',
        detail: `${install.file} \u{b7} ${install.reason}`,
        percent: null,
      };
    case 'unknown':
      return {
        mark: 'off',
        title: 'the download state is one this client cannot read',
        detail: 'this client is older than the forge serving it',
        percent: null,
      };
  }
}

/** The activation's line, or `null` when none is running. */
export function activateLine(activate: ActivateState): OpLine | null {
  switch (activate.state) {
    case 'idle':
      return null;
    case 'activating':
      return {
        mark: 'live',
        title: `loading ${activate.file}`,
        detail: `${roleWord(activate.role)} \u{b7} dictation keeps running the current model until this one is up`,
        percent: null,
      };
    case 'failed':
      return {
        mark: 'failed',
        title: 'the model did not load',
        detail: `${activate.file} \u{b7} ${activate.reason} \u{b7} the current model is still running`,
        percent: null,
      };
    case 'unknown':
      return {
        mark: 'off',
        title: 'the activation state is one this client cannot read',
        detail: 'this client is older than the forge serving it',
        percent: null,
      };
  }
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
  // A model downloaded from the feed's docs has no published digest, and a
  // row that printed a placeholder would claim bytes nobody checked.
  if (model.sha256 !== null) pinned.push({ text: `sha ${model.sha256.slice(0, 8)}` });
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
