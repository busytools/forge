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
  BenchMetrics,
  BenchResult,
  BenchRole,
  BenchState,
  BenchTarget,
  BenchTier,
  CandidateRow,
  CatalogueCheck,
  CatalogueRow,
  InstallState,
  InstalledModel,
  InUseModel,
  ModelRole,
  ModelUpdate,
  ReadAloudRecording,
  UpdateVerdict,
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
 * What one comparison row says about its candidate, in the column order the
 * table draws: the speed against the model in use, the error the same way,
 * the licence, and what the rule makes of it.
 */
export interface ComparisonRow {
  /** The feed's own row, for the controls that act on it. */
  source: CatalogueRow;
  variant: string;
  display_name: string;
  /** The speed axis, against the model in use. */
  speed: string | null;
  /** The error axis, against the model in use. */
  error: string | null;
  license: string | null;
  /** The verdict's own sentence. */
  verdict: string;
  /** The verdict's own word, for the row's chip. */
  mark: string;
  recommended: boolean;
}

/**
 * The rule's verdict as the table states it, in plain words.
 *
 * **A non-commercial licence is said here, not filtered out.** A
 * recommendation that hid the licence would be asking the reader to
 * discover it after the download.
 */
export function verdictWord(verdict: UpdateVerdict): string {
  switch (verdict) {
    case 'recommended':
      return 'recommended';
    case 'beats_both':
      return 'also beats both, but slower';
    case 'slower':
      return 'slower than this';
    case 'blunter':
      return 'no more accurate';
    case 'unknown':
      return 'a verdict this client cannot read';
  }
}

/** One update's rows, in the rule's own order, with the baseline's numbers. */
export function comparison(update: ModelUpdate): ComparisonRow[] {
  return update.candidates.map((candidate) => {
    const row = candidate.row;
    return {
      source: row,
      variant: row.variant,
      display_name: row.display_name,
      speed:
        row.speed === null
          ? null
          : `${speedLabel(row.speed.xrt_wall)} vs ${speedLabel(update.current.speed_x)}`,
      error: row.wer === null ? null : `${row.wer.err_pct}% vs ${update.current.fleurs_en_wer}%`,
      license: row.license,
      verdict: verdictWord(candidate.verdict),
      mark: candidate.verdict === 'recommended' ? 'ok' : 'off',
      recommended: candidate.verdict === 'recommended',
    };
  });
}

/** The candidate the feed recommends for one role, when there is one. */
export function recommendation(update: ModelUpdate): CandidateRow | null {
  return update.candidates.find((candidate) => candidate.verdict === 'recommended') ?? null;
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

/** The bench's own role word, which is the vocabulary the section uses. */
export function benchRoleWord(role: BenchRole): string {
  switch (role) {
    case 'transcribing':
      return 'transcribing';
    case 'cleanup':
      return 'cleanup';
    case 'other':
      return 'model';
  }
}

/** The tier's own word, as the section names what a run scores against. */
export function tierWord(tier: BenchTier): string {
  switch (tier) {
    case 'consensus':
      return 'your own takes';
    case 'read_aloud':
      return 'the read-aloud passage';
    case 'other':
      return 'a corpus this client cannot name';
  }
}

/** One row of the bench list: the target, and what the page knows about it. */
export interface BenchRow {
  target: BenchTarget;
  /** The variant the feed calls it, where one is known - the join a
   * recommendation matches on. */
  variant: string | null;
  /** This is the model its role runs right now. */
  current: boolean;
  /** The feed proposes this model for one of the roles. */
  recommended: boolean;
}

/**
 * The models a bench can run, from what the page holds: the roles' models
 * that are loaded, and the installed set. Each row says whether it is the
 * model in use and whether the feed recommends it, so a run is read against
 * what it would replace.
 *
 * A model in use that has not loaded is left out - the bench loads its own
 * engine from the file, and a control over a file that is not there yet
 * would fail where the page could have pointed at the progress instead.
 */
export function benchTargets(
  inUse: InUseModel[],
  installed: InstalledModel[],
  updates: ModelUpdate[],
): BenchRow[] {
  const rows: BenchRow[] = inUse
    .filter((model) => model.state === 'ready')
    .map((model) => ({
      target: {
        file: model.file,
        role: model.role === 'normalization' ? 'cleanup' : model.role,
        pinned: model.from.from === 'config',
      },
      variant: model.catalogue?.variant ?? null,
      current: true,
      recommended: false,
    }));
  for (const record of installed) {
    if (rows.some((row) => row.target.file === record.file)) continue;
    rows.push({
      target: { file: record.file, role: 'transcribing', pinned: false },
      variant: record.variant,
      current: false,
      recommended: false,
    });
  }
  for (const row of rows) {
    row.recommended = updates.some((update) =>
      update.candidates.some(
        (candidate) => candidate.verdict === 'recommended' && candidate.row.variant === row.variant,
      ),
    );
  }
  return rows;
}

/** The bench's line: what is running, or what stopped it. */
export function benchLine(bench: BenchState): OpLine | null {
  switch (bench.state) {
    case 'idle':
      return null;
    case 'running': {
      const percent = bench.clips > 0 ? Math.floor((bench.clip / bench.clips) * 100) : null;
      const share =
        bench.so_far === null ? '' : ` \u{b7} ${Math.round(bench.so_far * 100)}% agreed so far`;
      return {
        mark: 'live',
        title: `benching ${bench.target.file}`,
        detail: `clip ${bench.clip} of ${bench.clips} \u{b7} ${tierWord(bench.tier)}${share}`,
        percent,
      };
    }
    case 'failed':
      return {
        mark: 'failed',
        title: 'the bench did not finish',
        detail: `${bench.target.file} \u{b7} ${bench.reason}`,
        percent: null,
      };
    case 'unknown':
      return {
        mark: 'off',
        title: 'the bench state is one this client cannot read',
        detail: 'this client is older than the forge serving it',
        percent: null,
      };
  }
}

/**
 * One result's facts, in the order a reader compares two runs: the speed,
 * the error figure, the agreement, then what was scored.
 *
 * Term accuracy is the headline where a run has one - it is the only
 * figure scored against words known to be true - and where it does not,
 * the line says so rather than printing a zero.
 */
export function resultFacts(result: BenchResult): FactPart[] {
  const metrics = result.metrics;
  const stages = metrics.stages_ms;
  const parts: FactPart[] = [
    { text: `${metrics.xrt_wall.toFixed(1)}\u{d7} realtime`, hl: true },
    { text: `${metrics.wall_seconds.toFixed(1)}s wall` },
  ];
  if (metrics.term_accuracy !== null) {
    parts.push({ text: `term accuracy ${Math.round(metrics.term_accuracy * 100)}%`, hl: true });
  }
  if (metrics.wer !== null) {
    parts.push({ text: `WER ${(metrics.wer * 100).toFixed(1)}%` });
  }
  if (metrics.matched !== null) {
    const [agreed, of] = metrics.matched;
    parts.push({ text: `${agreed} of ${of} matched a baseline` });
  }
  parts.push({
    text: `${metrics.clips} clips \u{b7} ${Math.round(metrics.audio_seconds)}s of audio`,
  });
  // Where the wall time went, so two runs can be compared past the totals.
  parts.push({
    text: `load ${stages.model_load_ms}ms \u{b7} encode ${stages.encode_ms}ms \u{b7} decode ${stages.decode_ms}ms \u{b7} cleanup ${stages.normalize_ms}ms`,
  });
  return parts;
}

/** The rule, one sentence, for the table's own caption. */
export function updateWhy(): string {
  return 'Every English model the feed measured on both axes, fastest first. The rule takes the first that beats the model in use on both - speed and error - and the rows under it say which axis each one loses on.';
}

/**
 * The licence the model in use carries, for the comparison table's baseline
 * row. The row draws a licence like every other row; the words saying it is
 * what runs live in the rule column.
 */
export function inUseLicense(inUse: InUseModel[], role: ModelRole): string {
  return inUse.find((model) => model.role === role)?.facts.license ?? 'no licence on the feed';
}

/** A recording's length, as its own row reads it: `m:ss`. */
export function recordingLength(recording: ReadAloudRecording): string {
  const seconds = Math.round(recording.duration_ms / 1000);
  return `${Math.floor(seconds / 60)}:${String(seconds % 60).padStart(2, '0')}`;
}

/** When a recording was made, as its own row reads it. */
export function recordingWhen(recording: ReadAloudRecording): string | null {
  const at = clock(recording.at);
  return at === null ? null : `recorded ${at}`;
}

/** When one result ran, as the row's own line. */
export function resultWhen(result: BenchResult): string | null {
  const at = clock(result.at);
  return at === null ? null : `measured ${at}`;
}

/**
 * What one result means against the model in use, which is the question a
 * bench exists to answer.
 *
 * **Only two runs over the SAME corpus can be compared** - the identity is
 * the corpus's own hash, so a take landing between two runs makes them
 * different corpora and this says so rather than pretending. Term accuracy
 * decides where both sides have it (the read-aloud tier); otherwise the
 * agreement share does, and speed is the tie the row already carries.
 */
export function resultVerdict(
  result: BenchResult,
  inUse: InUseModel[],
  results: BenchResult[],
): string {
  const role = result.target.role === 'cleanup' ? 'normalization' : result.target.role;
  const current = inUse.find((model) => model.role === role);
  if (current === undefined) {
    return 'this role is not running a model right now';
  }
  if (current.file === result.target.file) {
    return 'this is the model in use';
  }
  const other = results.find(
    (row) => row.target.file === current.file && row.corpus.sha256 === result.corpus.sha256,
  );
  if (other === undefined) {
    return `no run of ${current.file} over this same corpus to compare with yet`;
  }
  return verdictAgainst(result.metrics, other.metrics);
}

/** The comparison itself, from two metric sets over one corpus. */
function verdictAgainst(candidate: BenchMetrics, current: BenchMetrics): string {
  const speed = `${candidate.xrt_wall.toFixed(1)}\u{d7} vs ${current.xrt_wall.toFixed(1)}\u{d7}`;
  if (candidate.term_accuracy !== null && current.term_accuracy !== null) {
    const better = candidate.term_accuracy > current.term_accuracy;
    const share = `${Math.round(candidate.term_accuracy * 100)}% vs ${Math.round(current.term_accuracy * 100)}% of the passage's terms`;
    return better
      ? `ahead of the model in use: ${share} survived, ${speed}`
      : candidate.term_accuracy < current.term_accuracy
        ? `behind the model in use: ${share} survived, ${speed}`
        : `level with the model in use on term accuracy: ${share}, ${speed}`;
  }
  if (candidate.matched !== null && current.matched !== null) {
    const [agreed, of] = candidate.matched;
    const [agreedNow, ofNow] = current.matched;
    const share = `${agreed} of ${of} vs ${agreedNow} of ${ofNow} matched a baseline`;
    if (agreed * ofNow > agreedNow * of)
      return `agrees with the baselines more often than the model in use (${share}, ${speed})`;
    if (agreed * ofNow < agreedNow * of)
      return `agrees with the baselines less often than the model in use (${share}, ${speed})`;
    return `level with the model in use on agreement (${share}, ${speed})`;
  }
  return `no comparable figure against the model in use (${speed})`;
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
