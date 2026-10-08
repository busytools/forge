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
  CatalogueKind,
  CatalogueRow,
  DictateModelsWire,
  InstallState,
  InstalledModel,
  InUseModel,
  ModelRole,
  ModelUpdate,
  ReadAloudRecording,
  UpdateVerdict,
} from '../wire/models';
import type { DictateModelState } from '../wire/types';

/**
 * A size in the units the number is read in: the catalogue quotes models in
 * megabytes, and a read-aloud recording is seconds long, where a whole
 * megabyte rounds a real recording down to `0 MB`.
 */
export function sizeLabel(bytes: number): string {
  if (bytes >= 1_000_000_000) return `${(bytes / 1_000_000_000).toFixed(2)} GB`;
  if (bytes < 1_000_000) return `${Math.round(bytes / 1_000)} KB`;
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
 * The candidate's facts as its recommendation draws them: what it costs,
 * what the feed measured against the model in use, its licence, and what it
 * reads. Every part is the feed's own figure; a fact it did not carry is no
 * part at all.
 */
export function updateFacts(update: ModelUpdate, row: CatalogueRow): FactPart[] {
  const parts: FactPart[] = row.params === null ? [] : [{ text: paramsLabel(row.params) }];

  if (row.download !== null) {
    parts.push({ text: `${row.download.quant} ${sizeLabel(row.download.size_bytes)}` });
  }
  if (row.speed !== null) {
    parts.push({
      text: `${speedLabel(row.speed.xrt_wall)} vs ${speedLabel(update.current.speed_x)}`,
      hl: true,
    });
  }
  if (row.wer !== null) {
    parts.push({
      text: `${row.wer.dataset.toUpperCase()}-${row.wer.language} ${row.wer.err_pct} vs ${update.current.fleurs_en_wer}`,
    });
  }
  if (row.license !== null) parts.push({ text: row.license });

  const languages = languagesLabel(row.languages);
  parts.push({
    text: [languages, row.streaming ? 'streaming' : 'offline']
      .filter((word) => word !== null)
      .join(', '),
  });

  return parts;
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
export function entryUrl(row: CatalogueRow): string {
  // The Hub's entries name their own page; the speech feed's are documents
  // in its catalogue tree, which is where the server fetches the same file
  // from.
  return (
    row.url ??
    `https://github.com/handy-computer/transcribe.cpp/blob/main/catalog/${encodeURIComponent(row.variant)}.json`
  );
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
 * offers nothing. **The row's kind names the role its control loads**: a
 * normalizer's candidate activates the cleanup role, a speech row's
 * transcribing.
 */
export type RowAction =
  | { do: 'install'; label: string }
  | { do: 'activate'; label: string; file: string; role: ModelRole }
  | { do: 'off'; label: string }
  | { do: 'none' };

export function rowAction(
  row: CatalogueRow,
  installed: InstalledModel[],
  inUse: InUseModel[],
): RowAction {
  // A row's own kind says which role it fills: a normalizer's control loads
  // the cleanup role, and a speech row's loads transcribing.
  const role: ModelRole = row.kind === 'normalizer' ? 'normalization' : 'transcribing';
  const active = inUse.find((model) => model.role === role) ?? null;
  const record = installed.find((model) => model.variant === row.variant);
  if (record !== undefined) {
    if (active?.file === record.file) return { do: 'off', label: 'active' };
    if (active?.from.from === 'config') return { do: 'off', label: 'installed' };
    return { do: 'activate', label: `use for ${roleWord(role)}`, file: record.file, role };
  }
  if (row.download === null) return { do: 'none' };
  return { do: 'install', label: `install ${row.download.quant}` };
}

/**
 * One run's headline, for the line under the candidate it belongs to: the
 * speed, the figure that carries a verdict, and the agreement - and nothing
 * else.
 *
 * The stage timings and the corpus's own shape stay on the bench section's
 * result card, where a reader is diagnosing one run. Under a candidate they
 * are the same numbers on every row, which is what turns a comparison into a
 * wall of figures.
 */
export function resultHeadline(result: BenchResult): FactPart[] {
  const metrics = result.metrics;
  const parts: FactPart[] = [{ text: `${metrics.xrt_wall.toFixed(1)}\u{d7} realtime`, hl: true }];
  if (metrics.term_accuracy !== null) {
    parts.push({ text: `term accuracy ${Math.round(metrics.term_accuracy * 100)}%`, hl: true });
  } else if (metrics.wer !== null) {
    parts.push({ text: `WER ${(metrics.wer * 100).toFixed(1)}%` });
  }
  if (metrics.matched !== null) {
    parts.push({ text: `${String(metrics.matched[0])} of ${String(metrics.matched[1])} matched` });
  }
  return parts;
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
        // A failure before the file is named has no file to lead with, and a
        // line that began with the separator would read as a named one.
        detail: install.file === '' ? install.reason : `${install.file} \u{b7} ${install.reason}`,
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

/** One of the feed's families, and how many entries it holds. */
export interface Family {
  name: string;
  count: number;
}

/**
 * The classes the feed holds, most-populated first and then by name.
 *
 * The box is blind on its own - a reader who does not already know a model
 * name has nothing to type - and the family is the feed's own word for what
 * a thing is.
 *
 * **Each count is `search`'s own answer for that name**, so the number on a
 * chip is always the number the click will draw. Counting exact family
 * membership instead would promise fewer rows than the click shows wherever
 * one family's name is a substring of a sibling's - the feed's `moonshine`
 * and `moonshine-streaming` are the live pair. A familyless row contributes
 * no chip: its name matches nothing, so a chip for it would be a control
 * that does nothing.
 */
export function families(rows: CatalogueRow[]): Family[] {
  const names = new Set(rows.map((row) => row.family).filter((name) => name !== ''));
  return [...names]
    .map((name) => ({ name, count: search(rows, name).length }))
    .sort((a, b) => b.count - a.count || a.name.localeCompare(b.name));
}

/**
 * The fastest entries the feed measured, which is the ranking this page can
 * stand on: the feed's documents do carry `measured_on` dates, but nothing
 * forge reads keeps one - `SpeedRow` drops them - so "latest" is not
 * something this read can say, and a row with no measured speed is not a
 * recommendation either.
 */
export function fastest(rows: CatalogueRow[], take: number): CatalogueRow[] {
  return rows
    .filter((row) => row.speed !== null)
    .sort((a, b) => (b.speed?.xrt_wall ?? 0) - (a.speed?.xrt_wall ?? 0))
    .slice(0, take);
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
 *
 * **An installed record's role comes off the feed row it joined**: the
 * record itself carries no kind, and a normalizer benched as transcribing
 * would load into the wrong slot.
 */
export function benchTargets(
  inUse: InUseModel[],
  installed: InstalledModel[],
  updates: ModelUpdate[],
  feed: CatalogueRow[] = [],
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
    const entry = feed.find((row) => row.variant === record.variant);
    // The feed's row when the catalogue is read, the record's own runtime
    // when it is not - the same join `roleModels` makes, so the two lists
    // cannot disagree about what a file is.
    const kind = entry === undefined || entry.kind === 'other' ? recordKind(record) : entry.kind;
    rows.push({
      target: {
        file: record.file,
        role: kind === 'normalizer' ? 'cleanup' : 'transcribing',
        pinned: false,
      },
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

/**
 * What a recorded model is for, when the feed cannot say.
 *
 * **The record declares its own runtime**, and the runtime is what the role
 * loads it with: llama.cpp is the cleanup stage's generator and
 * transcribe.cpp the transcriber. With the catalogue unread - a first boot
 * offline, a check that never landed - this is the join the page still has,
 * and it is the SAME one for every list: a record that declares neither is
 * not offered as a role's model at all, because a guessed role is a model
 * running in the wrong slot.
 */
export function recordKind(record: InstalledModel): Exclude<CatalogueKind, 'other'> | null {
  if (record.facts.runtime === 'llama.cpp') return 'normalizer';
  if (record.facts.runtime === 'transcribe.cpp') return 'asr';
  return null;
}

/** One run a sweep will make. */
export interface SweepRun {
  variant: string;
  role: BenchRole;
  /** The file, when this machine has it: what a bench target names and what
   * an uninstall would remove. `null` until a run's own install lands, since
   * the feed names a variant and the install is what resolves its file. */
  file: string | null;
  /** The bytes its download costs, so the card can price a press. */
  size_bytes: number;
  /** Whether its bytes were already on this machine when the plan was drawn.
   * A run that was already here is never downloaded by the sweep, and never
   * removed by it either. */
  installed: boolean;
  /** What it is for: the role's pick, a candidate the bench has to decide,
   * or the model in use read against. */
  why: 'pick' | 'candidate' | 'baseline';
}

/** What one press of the sweep button will do, before it does any of it. */
export interface SweepPlan {
  runs: SweepRun[];
  /** The tier every run scores on: the read-aloud set when one is recorded,
   * the takes otherwise. */
  tier: BenchTier;
  /** The bytes that come down the wire: what is not already here. */
  bytes: number;
  /** The candidates the sweep will NOT reach, so the verdict can carry its
   * own scope: "best among these", never a bare best. */
  beyond: number;
  /** The runtime a comparable run last measured, when one exists - the honest
   * answer to "how long will this take". */
  seconds_runs: number | null;
}

/** How many cleanup candidates a press takes. Downloads is the only pre-run
 * signal a normalizer has, so the depth is by popularity, and the verdict
 * says what it left out. */
export const SWEEP_DEPTH = 3;

/**
 * The runs one press makes: the transcribing side's own pick, the cleanup
 * role's most-downloaded candidates, and each role's model in use as the
 * baseline the others are read against.
 *
 * **A sweep cannot promise the best model; it promises the best of what it
 * ran.** Downloads orders the candidates because nothing else exists before
 * a run, and the plan carries how many were left out so no verdict can say a
 * bare "best".
 */
export function sweepPlan(wire: DictateModelsWire): SweepPlan {
  const tier: BenchTier = wire.read_aloud.recordings.length > 0 ? 'read_aloud' : 'consensus';
  const runs: SweepRun[] = [];
  const add = (
    role: BenchRole,
    variant: string,
    file: string | null,
    size_bytes: number,
    why: SweepRun['why'],
  ): void => {
    if (runs.some((run) => run.role === role && run.variant === variant)) return;
    runs.push({ variant, role, file, size_bytes, installed: file !== null, why });
  };

  const update = wire.updates.find((held) => held.role === 'transcribing');
  const pick = update === undefined ? null : recommendation(update);
  if (pick !== null) {
    const record = wire.installed.find((model) => model.variant === pick.row.variant);
    add(
      'transcribing',
      pick.row.variant,
      record?.file ?? null,
      record?.size ?? pick.row.download?.size_bytes ?? 0,
      'pick',
    );
  }

  // Each baseline is the FILE the role runs, not the installed record under
  // the same variant: a pin can run one quant while the store holds another,
  // and the verdict is about what runs.
  const running = wire.in_use.find((model) => model.role === 'transcribing');
  if (running !== undefined) {
    add(
      'transcribing',
      running.catalogue?.variant ?? running.file,
      running.file,
      running.size,
      'baseline',
    );
  }
  const inUse = wire.in_use.find((model) => model.role === 'normalization');
  if (inUse !== undefined) {
    add('cleanup', inUse.catalogue?.variant ?? inUse.file, inUse.file, inUse.size, 'baseline');
  }

  const ranked = wire.rows
    .filter((row) => row.kind === 'normalizer')
    .sort((a, b) => (b.download_count ?? 0) - (a.download_count ?? 0));
  for (const row of ranked.slice(0, SWEEP_DEPTH)) {
    const record = wire.installed.find((model) => model.variant === row.variant);
    add(
      'cleanup',
      row.variant,
      record?.file ?? null,
      record?.size ?? row.download?.size_bytes ?? 0,
      'candidate',
    );
  }
  const beyond = Math.max(0, ranked.length - SWEEP_DEPTH);

  const bytes = runs.filter((run) => !run.installed).reduce((sum, run) => sum + run.size_bytes, 0);
  // The last comparable run's wall time: the corpus is the same shape as the
  // sweep's, so its clock is the honest estimate.
  const comparable = wire.results.filter((result) => result.tier === tier);
  const seconds_runs =
    comparable.length === 0 ? null : (comparable[0]?.metrics.wall_seconds ?? null);

  return { runs, tier, bytes, beyond, seconds_runs };
}

/** What one sweep's runs say, per role: the best of them, the baseline it is
 * read against, and the scope of the claim. */
export interface SweepVerdict {
  role: BenchRole;
  /** The run that read best, and its result. */
  best: { run: SweepRun; result: BenchResult };
  /** The model in use's own run on the same corpus, when the sweep has one. */
  baseline: BenchResult | null;
  /** Whether the model in use is the one that read best. */
  onBest: boolean;
  /** How many runs were scored, and how many candidates were never tried. */
  scored: number;
  beyond: number;
  /** The candidates this role's sweep ran - the scope's own number. */
  tried: number;
  /** Whether the plan carried a feed pick for this role, which the scope has
   * to be able to say: a sweep with nothing proposed scored the model in use
   * alone, and calling that "the feed's own pick" is a claim nobody made. */
  pick: boolean;
  tier: BenchTier;
}

/**
 * Read one sweep's own results into a verdict, per role.
 *
 * Only runs that share ONE tier and ONE corpus are compared - a number from
 * another corpus is not a comparison - and the baseline is the in-use
 * model's own run from that same set, which is why the sweep benches it too.
 */
export function sweepVerdicts(wire: DictateModelsWire, plan: SweepPlan): SweepVerdict[] {
  const filed = plan.runs.filter((run) => run.file !== null);
  const results = wire.results.filter(
    (result) => filed.some((run) => run.file === result.target.file) && result.tier === plan.tier,
  );
  if (results.length === 0) return [];
  // The corpus the sweep actually produced: the newest run's, which every run
  // in one sweep shares.
  const corpus = results[0]?.corpus.sha256;
  const mine = results.filter((result) => result.corpus.sha256 === corpus);

  const verdicts: SweepVerdict[] = [];
  for (const role of ['transcribing', 'cleanup'] as const) {
    const scored: { run: SweepRun; result: BenchResult }[] = [];
    for (const run of plan.runs) {
      if (run.file === null || run.role !== role) continue;
      const result = mine.find((row) => row.target.file === run.file);
      if (result !== undefined) scored.push({ run, result });
    }
    if (scored.length === 0) continue;
    const best = scored.reduce((a, b) => (readsBetter(b.result, a.result) ? b : a));
    const baseline = scored.find((entry) => entry.run.why === 'baseline')?.result ?? null;
    verdicts.push({
      role,
      best,
      baseline,
      onBest: baseline !== null && baseline.target.file === best.run.file,
      scored: scored.length,
      beyond: role === 'cleanup' ? plan.beyond : 0,
      tried: plan.runs.filter((run) => run.role === role && run.why === 'candidate').length,
      pick: plan.runs.some((run) => run.role === role && run.why === 'pick'),
      tier: plan.tier,
    });
  }
  return verdicts;
}

/** One verdict's headline: what read best, against what. */
/**
 * The verdict's headline, in the words the tier can stand on.
 *
 * **A take-scored run reads agreement, not quality**: its corpus has no
 * known words, so "read better" would claim a reading nobody measured - the
 * same rule that keeps a faster normalizer from being the cleanup pick.
 */
export function sweepHeadline(verdict: SweepVerdict): string {
  const role = verdict.role === 'cleanup' ? 'cleanup model' : 'transcribing model';
  const file = verdict.best.result.target.file;
  const best = verdict.tier === 'read_aloud' ? 'read best' : 'agreed most';
  const better =
    verdict.tier === 'read_aloud'
      ? `read better than the ${role} you run`
      : `agreed with the baselines more often than the ${role} you run`;
  if (verdict.baseline === null) return `${file} ${best} of the ${verdict.scored} scored`;
  if (verdict.onBest) return `the ${role} you run ${best} of the ${verdict.scored} scored`;
  return `${file} ${better}`;
}

/** The verdict's own scope, so a "best" always names what it saw. */
export function sweepScope(verdict: SweepVerdict): string {
  const clips = verdict.best.result.corpus.clips;
  const corpus = verdict.tier === 'read_aloud' ? 'your read-aloud set' : 'your takes';
  const where = `${corpus}, ${clips} ${clips === 1 ? 'clip' : 'clips'}`;
  if (verdict.role !== 'cleanup') {
    return verdict.pick
      ? `the feed's own pick, scored on ${where}`
      : `only the model you run, scored on ${where} - nothing else was proposed`;
  }
  if (verdict.tried === 0) {
    return `only the model you run, scored on ${where} - no cleanup candidate was in the plan`;
  }
  const tried =
    verdict.tried === 1
      ? 'the most-downloaded cleanup candidate'
      : `the ${verdict.tried} most-downloaded cleanup candidates`;
  const left =
    verdict.beyond === 0 ? '' : ` \u{b7} ${verdict.beyond} more candidates were not tried`;
  return `best of ${tried}, scored on ${where}${left}`;
}

/** What switching a role to a variant would cost now: the bytes the sweep
 * took back on its way out, when they must come down again. */
export function sweepCost(wire: DictateModelsWire, variant: string): number | null {
  if (wire.installed.some((model) => model.variant === variant)) return 0;
  const row = wire.rows.find((entry) => entry.variant === variant);
  return row?.download?.size_bytes ?? null;
}

/** One role's models, as the dictation panel offers them. */
export interface RoleModels {
  /** The model the role runs now, when it runs one. */
  current: InUseModel | null;
  /** Whether `forge.toml` sets the role, which no runtime press can move. */
  pinned: boolean;
  /** The key that sets it, named so a reader knows what to remove. */
  pinKey: string | null;
  /** Every model this machine has that could take the role, the current one
   * first. */
  choices: { file: string; current: boolean }[];
}

/**
 * The models one role can run.
 *
 * **An installed record carries no role of its own**, so the join is the
 * feed's row for the variant it was recorded under - the same join the bench
 * list makes. A record whose variant the feed does not carry is left to the
 * role it already runs rather than guessed at, and the model the role runs
 * now is always offered so the list reads as a list rather than as a
 * difference.
 */
export function roleModels(
  role: ModelRole,
  inUse: InUseModel[],
  installed: InstalledModel[],
  rows: CatalogueRow[],
): RoleModels {
  const current = inUse.find((model) => model.role === role) ?? null;
  const wanted: Exclude<CatalogueKind, 'other'> = role === 'normalization' ? 'normalizer' : 'asr';
  const choices: { file: string; current: boolean }[] = [];
  if (current !== null) choices.push({ file: current.file, current: true });
  for (const record of installed) {
    if (choices.some((choice) => choice.file === record.file)) continue;
    const entry = rows.find((row) => row.variant === record.variant);
    // The same join `benchTargets` makes: the feed's row when the catalogue
    // is read, the record's own runtime when it is not, and nothing at all
    // when the record declares neither.
    const kind = entry === undefined || entry.kind === 'other' ? recordKind(record) : entry.kind;
    if (kind !== wanted) continue;
    choices.push({ file: record.file, current: false });
  }
  return {
    current,
    pinned: current?.from.from === 'config',
    pinKey: current !== null && current.from.from === 'config' ? current.from.key : null,
    choices,
  };
}

/** One cleanup candidate as its row draws: the feed's row, this machine's
 * record of it, and what the bench has measured on it. */
export interface CleanupCandidate {
  row: CatalogueRow;
  installed: InstalledModel | null;
  results: BenchResult[];
}

/** The cleanup role's candidates: the feed's normalizer rows, each joined to
 * this machine's record of it and to that record's own bench results. */
export function cleanupCandidates(
  rows: CatalogueRow[],
  installed: InstalledModel[],
  results: BenchResult[],
): CleanupCandidate[] {
  return rows
    .filter((row) => row.kind === 'normalizer')
    .map((row) => {
      const record = installed.find((model) => model.variant === row.variant) ?? null;
      return {
        row,
        installed: record,
        results:
          record === null
            ? []
            : results.filter(
                (result) => result.target.role === 'cleanup' && result.target.file === record.file,
              ),
      };
    });
}

/**
 * The cleanup role's own proposal: the candidate the bench measured best.
 *
 * **The feed cannot rank this role** - it publishes no speed and no error
 * for a normalizer - so the bench does. Two rules keep that honest: only
 * runs carrying an ACCURACY figure are compared, because the takes tier has
 * no known words and a faster normalizer is not a better one; and two runs
 * are only compared on one tier and one corpus, so the pick comes from the
 * corpus this role has the most such results on, at least two of them. No
 * pick otherwise - one run is a number, not a comparison, and the card says
 * so rather than proposing the only thing it has.
 */
export function cleanupPick(
  candidates: CleanupCandidate[],
): { candidate: CleanupCandidate; result: BenchResult } | null {
  const groups = new Map<string, { candidate: CleanupCandidate; result: BenchResult }[]>();
  for (const candidate of candidates) {
    for (const result of candidate.results) {
      if (result.metrics.wer === null && result.metrics.term_accuracy === null) continue;
      const key = `${result.tier}/${result.corpus.sha256}`;
      groups.set(key, [...(groups.get(key) ?? []), { candidate, result }]);
    }
  }
  const widest = [...groups.values()].sort((a, b) => b.length - a.length)[0];
  if (widest === undefined || widest.length < 2) return null;
  return widest.reduce((best, entry) => (readsBetter(entry.result, best.result) ? entry : best));
}

/**
 * Whether one run reads better than another **on the same corpus**: the
 * lower word error first, then the higher term accuracy, then the faster -
 * the order a person comparing two of their own runs reads them in.
 */
function readsBetter(a: BenchResult, b: BenchResult): boolean {
  if (a.metrics.wer !== null && b.metrics.wer !== null && a.metrics.wer !== b.metrics.wer) {
    return a.metrics.wer < b.metrics.wer;
  }
  if (
    a.metrics.term_accuracy !== null &&
    b.metrics.term_accuracy !== null &&
    a.metrics.term_accuracy !== b.metrics.term_accuracy
  ) {
    return a.metrics.term_accuracy > b.metrics.term_accuracy;
  }
  // The takes tier's own signal: a run that agrees with the baselines more
  // often read closer to what was said, where speed alone is not a reading
  // at all - the same rule the cleanup pick refuses to make without it.
  if (a.metrics.matched !== null && b.metrics.matched !== null) {
    const [agreedA, ofA] = a.metrics.matched;
    const [agreedB, ofB] = b.metrics.matched;
    if (agreedA * ofB !== agreedB * ofA) return agreedA * ofB > agreedB * ofA;
  }
  return a.metrics.xrt_wall > b.metrics.xrt_wall;
}

/**
 * What a failed bench is failing AT: its target and its reason, so a
 * dismissal, or a sweep deciding whether a failure is one of its own, is
 * keyed by the failure rather than by failure having happened.
 */
export function failureKey(bench: BenchState): string | null {
  return bench.state === 'failed' ? `${bench.target.file}|${bench.reason}` : null;
}

/** [`failureKey`] for the install's own failed state, on the same terms. */
export function installKey(install: InstallState): string | null {
  return install.state === 'failed' ? `${install.file}|${install.reason}` : null;
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
    text: `${metrics.clips} ${metrics.clips === 1 ? 'clip' : 'clips'} \u{b7} ${Math.round(metrics.audio_seconds)}s of audio`,
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

/**
 * When a recording was made, as its own row reads it - to the second, because
 * two recordings of the passage are a minute apart at most and the row is
 * what tells one from the other.
 */
export function recordingWhen(recording: ReadAloudRecording): string | null {
  const at = new Date(recording.at);
  if (Number.isNaN(at.getTime())) return null;
  const two = (part: number) => String(part).padStart(2, '0');
  return `recorded ${two(at.getHours())}:${two(at.getMinutes())}:${two(at.getSeconds())}`;
}

/**
 * One recording's facts, as its row draws them beside the length: what it
 * costs on disk, the digest that identifies those exact samples - the same
 * shorthand the in-use rows print - and when it was made.
 */
export function recordingFacts(recording: ReadAloudRecording): FactPart[] {
  const parts: FactPart[] = [{ text: sizeLabel(recording.bytes) }];
  if (recording.sha256 !== '') parts.push({ text: `sha ${recording.sha256.slice(0, 8)}` });
  const when = recordingWhen(recording);
  if (when !== null) parts.push({ text: when });
  return parts;
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

/**
 * The comparison itself, from two metric sets over one corpus.
 *
 * **Only the model in use's own figures are named here**: the run's own are
 * the facts beside this sentence, and repeating them reads as two numbers
 * about two things.
 */
function verdictAgainst(candidate: BenchMetrics, current: BenchMetrics): string {
  const speed = `${current.xrt_wall.toFixed(1)}\u{d7}`;
  if (candidate.term_accuracy !== null && current.term_accuracy !== null) {
    const theirs = `${Math.round(current.term_accuracy * 100)}% of the passage's terms at ${speed}`;
    if (candidate.term_accuracy > current.term_accuracy) {
      return `ahead of the model in use, which read ${theirs}`;
    }
    if (candidate.term_accuracy < current.term_accuracy) {
      return `behind the model in use, which read ${theirs}`;
    }
    return `level with the model in use, which read ${theirs}`;
  }
  if (candidate.matched !== null && current.matched !== null) {
    const [agreed, of] = candidate.matched;
    const [agreedNow, ofNow] = current.matched;
    const theirs = `${agreedNow} of ${ofNow} baselines at ${speed}`;
    if (agreed * ofNow > agreedNow * of) {
      return `agrees with the baselines more often than the model in use, which matched ${theirs}`;
    }
    if (agreed * ofNow < agreedNow * of) {
      return `agrees with the baselines less often than the model in use, which matched ${theirs}`;
    }
    return `level with the model in use on agreement, which matched ${theirs}`;
  }
  return `nothing comparable against the model in use, which ran at ${speed}`;
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
