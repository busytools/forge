/**
 * The models page's snapshot, as `crates/forge-workspace/src/catalogue.rs`
 * writes it and `Subject::DictateModels` answers with.
 *
 * The read is the whole page: the pinned models with their live states, the
 * catalogue check, the entries worth adopting, and the feed's rows. A page
 * that had to ask the core for any of it separately would be a second read of
 * one subject.
 */

import { modelStateFrom, narrow, type DictateModelState } from './types';

/** What a model is for, as this page words it. `other` is a role this client is older than. */
export type ModelRole = 'transcribing' | 'normalization' | 'other';

/** Facts the pin declares about its checkpoint. */
export interface ModelFacts {
  quant: string | null;
  params: number | null;
  license: string | null;
  runtime: string | null;
}

/** One measured speed row, as the speed line draws it. */
export interface SpeedFact {
  machine: string;
  backend: string;
  quant: string;
  xrt_wall: number;
}

/** The preferred quantisation's download size. */
export interface DownloadFact {
  quant: string;
  size_bytes: number;
}

/** One accuracy figure and the benchmark it was measured on. */
export interface WerFact {
  dataset: string;
  split: string;
  language: string;
  err_pct: number;
}

/** What the feed says about a file the pin names. */
export interface CatalogueJoin {
  variant: string;
  display_name: string;
  /** The feed's own byte length for the file, against the pin's size. */
  size_bytes: number;
  streaming: boolean;
  languages: string[];
  speed: SpeedFact | null;
}

/**
 * Where a role's model came from.
 *
 * `config` is `forge.toml` naming the model, which the runtime cannot move;
 * `installed` is a model chosen on this machine; `pin` is the compiled
 * default. `unknown` is a source this client is older than.
 */
export type ActiveFrom =
  | { from: 'config'; key: string; variant: string }
  | { from: 'installed'; variant: string }
  | { from: 'pin' }
  | { from: 'unknown' };

/** One model forge runs: its declaration, its live state, and the feed's join. */
export interface InUseModel {
  role: ModelRole;
  file: string;
  size: number;
  /** The declared digest: `null` for an installed model whose upstream
   * publishes none, which is drawn without one. */
  sha256: string | null;
  state: DictateModelState;
  facts: ModelFacts;
  catalogue: CatalogueJoin | null;
  /** Where this role's model came from. */
  from: ActiveFrom;
  /** RFC 3339, when a runtime pick chose it. */
  at: string | null;
}

/** One model this machine has downloaded from the feed. */
export interface InstalledModel {
  variant: string;
  file: string;
  /** The doc's own URL, kept so a reader can see where the bytes came from. */
  url: string;
  size: number;
  facts: ModelFacts;
  /** RFC 3339. */
  at: string;
}

/**
 * Where the last model download got to. `unknown` is a state tag this client
 * is older than; drawing `idle` would claim nothing is downloading.
 */
export type InstallState =
  | { state: 'idle' }
  | { state: 'downloading'; file: string; got: number; total: number }
  | { state: 'failed'; file: string; reason: string }
  | { state: 'unknown' };

/**
 * Where the last model activation got to, with the same `unknown` rule as
 * [`InstallState`].
 */
export type ActivateState =
  | { state: 'idle' }
  | { state: 'activating'; role: ModelRole; file: string }
  | { state: 'failed'; role: ModelRole; file: string; reason: string }
  | { state: 'unknown' };

/** Which slot a bench target runs in. */
export type BenchRole = 'transcribing' | 'cleanup' | 'other';

/** One model a bench can load. */
export interface BenchTarget {
  file: string;
  role: BenchRole;
  pinned: boolean;
}

/** Which clips a run scores against, in the core's own names. */
export type BenchTier = 'consensus' | 'read_aloud' | 'other';

/** Where one run's time went, summed over its clips. */
export interface StageTotals {
  model_load_ms: number;
  resample_ms: number;
  mel_ms: number;
  encode_ms: number;
  decode_ms: number;
  normalize_ms: number;
}

/** What one run measured. */
export interface BenchMetrics {
  clips: number;
  audio_seconds: number;
  wall_seconds: number;
  xrt_wall: number;
  term_accuracy: number | null;
  wer: number | null;
  matched: [number, number] | null;
  stages_ms: StageTotals;
}

/** What two runs are comparable by. */
export interface CorpusId {
  clips: number;
  audio_seconds: number;
  sha256: string;
}

/** One finished bench, kept under what it was about. */
export interface BenchResult {
  target: BenchTarget;
  tier: BenchTier;
  metrics: BenchMetrics;
  at: string;
  corpus: CorpusId;
}

/** Where the last bench got to, with the same `unknown` rule as [`InstallState`]. */
export type BenchState =
  | { state: 'idle' }
  | {
      state: 'running';
      target: BenchTarget;
      tier: BenchTier;
      clip: number;
      clips: number;
      so_far: number | null;
    }
  | { state: 'failed'; target: BenchTarget; reason: string }
  | { state: 'unknown' };

/** One recording of the read-aloud passage, as the page lists it. */
export interface ReadAloudRecording {
  /** The take's directory name, which the page deletes by. */
  id: string;
  duration_ms: number;
  /** The wav's own byte length. */
  bytes: number;
  /** The wav's sha256, lowercase hex. */
  sha256: string;
  /** RFC 3339. */
  at: string;
}

/** The read-aloud set: the recordings this machine has, whether one is being
 * recorded right now, the passage they are read from, the terms a run scores
 * them on, and the last write's failure when there was one. */
export interface ReadAloudState {
  recordings: ReadAloudRecording[];
  recording: boolean;
  error: string | null;
  passage: string;
  terms: string[];
}

/**
 * What a catalogue entry is for: which role's candidates it belongs in. The
 * fourth arm is this client's own, for a kind a server newer than it sends.
 */
export type CatalogueKind = 'asr' | 'normalizer' | 'other';

/** One catalogue entry, as the candidate rows draw it. */
export interface CatalogueRow {
  variant: string;
  display_name: string;
  family: string;
  /** The parameter count the source published, or `null` when it published
   * none - a row drawing `0M params` would claim a measurement. */
  params: number | null;
  license: string | null;
  languages: string[];
  streaming: boolean;
  download: DownloadFact | null;
  speed: SpeedFact | null;
  wer: WerFact | null;
  kind: CatalogueKind;
  /** The entry's own page, where the feed names one. */
  url: string | null;
  /** How many times the Hub has served it - the only pre-run signal a
   * cleanup candidate carries. The speech feed leaves this `null`. */
  download_count: number | null;
}

/** Why a candidate is, or is not, the one the page proposes. */
export type UpdateVerdict = 'recommended' | 'beats_both' | 'slower' | 'blunter' | 'unknown';

/** One compared entry: the row, and what the rule makes of it. */
export interface CandidateRow {
  row: CatalogueRow;
  verdict: UpdateVerdict;
}

/**
 * The comparison one model in use is read against: its own numbers as the
 * baseline, and every comparable entry ranked fastest first.
 */
export interface ModelUpdate {
  role: ModelRole;
  file: string;
  current: { speed_x: number; fleurs_en_wer: number };
  candidates: CandidateRow[];
}

/**
 * What the last catalogue check did.
 *
 * `unknown` is this client's own: a `state` tag it does not know is a client
 * older than its server, and drawing `never` would claim nothing has fetched
 * on a machine that has. The page switches over this exhaustively, so a
 * server state added later is a compile error here rather than a row that
 * silently reads as fresh.
 */
export type CatalogueCheck =
  | { state: 'never' }
  | { state: 'checking' }
  | { state: 'fresh'; at: string; release: string | null; skipped: number }
  | { state: 'unreachable'; error: string }
  | { state: 'unknown' };

/** Everything the models page draws, in one read. */
export interface DictateModelsWire {
  enabled: boolean;
  models_dir: string | null;
  in_use: InUseModel[];
  check: CatalogueCheck;
  updates: ModelUpdate[];
  rows: CatalogueRow[];
  install: InstallState;
  activate: ActivateState;
  installed: InstalledModel[];
  bench: BenchState;
  results: BenchResult[];
  read_aloud: ReadAloudState;
}

/** The roles the core names. */
const ROLES: Exclude<ModelRole, 'other'>[] = ['transcribing', 'normalization'];

/** The check states the server writes; the fifth is this client's own. */
const CHECK_STATES = ['never', 'checking', 'fresh', 'unreachable'] as const;

/** The sources the core names for an active model; the fourth is this client's own. */
const FROM_SOURCES = ['config', 'installed', 'pin'] as const;

/** The install states the server writes; the fourth is this client's own. */
const INSTALL_STATES = ['idle', 'downloading', 'failed'] as const;

/** The activation states the server writes; the fourth is this client's own. */
const ACTIVATE_STATES = ['idle', 'activating', 'failed'] as const;

/** The bench states the server writes; the fourth is this client's own. */
const BENCH_STATES = ['idle', 'running', 'failed'] as const;

/** The verdicts the core writes; the fifth is this client's own. */
const VERDICTS: Exclude<UpdateVerdict, 'unknown'>[] = [
  'recommended',
  'beats_both',
  'slower',
  'blunter',
];

/** The bench tiers the core names; the last is this client's own. */
const BENCH_TIERS: Exclude<BenchTier, 'other'>[] = ['consensus', 'read_aloud'];

/** The entry kinds the core names; the last is this client's own. */
const KINDS: Exclude<CatalogueKind, 'other'>[] = ['asr', 'normalizer'];

/** The bench roles the core names; the fourth is this client's own. */
const BENCH_ROLES: Exclude<BenchRole, 'other'>[] = ['transcribing', 'cleanup'];

/**
 * The snapshot as the types above describe it, with every union member
 * narrowed HERE and not in a renderer.
 *
 * `checkLine`'s switch over `CatalogueCheck` is exhaustive so that a server
 * state added later fails a build until its line is written; a `default` arm
 * would trade that for a state that silently draws forever. So the unknown
 * value is caught where it enters, and the renderer keeps the compile-time
 * guarantee.
 */
export function modelsFrom(data: DictateModelsWire): DictateModelsWire {
  return {
    ...data,
    rows: (data.rows ?? []).map((row) => rowFrom(row)),
    in_use: data.in_use.map((model) => ({
      ...model,
      role: narrow(model.role, ROLES, 'other'),
      state: modelStateFrom(model.state),
      from: fromFrom(model.from),
    })),
    check: checkFrom(data.check),
    updates: data.updates.map((update) => ({
      ...update,
      role: narrow(update.role, ROLES, 'other'),
      candidates: (update.candidates ?? []).map((candidate) => ({
        ...candidate,
        row: rowFrom(candidate.row),
        verdict: narrow(candidate.verdict, VERDICTS, 'unknown'),
      })),
    })),
    install: tagged(data.install, INSTALL_STATES),
    activate: activateFrom(data.activate),
    bench: benchFrom(data.bench),
    results: (data.results ?? []).map((result) => ({
      ...result,
      target: targetFrom(result.target),
      tier: narrow(result.tier, BENCH_TIERS, 'other'),
    })),
    read_aloud: readAloudFrom(data.read_aloud),
  };
}

/** One catalogue row, with the kind narrowed the way the wire's roles are. */
function rowFrom(row: CatalogueRow): CatalogueRow {
  return {
    ...row,
    kind: narrow(row.kind, KINDS, 'other'),
    url: typeof row.url === 'string' ? row.url : null,
    download_count: typeof row.download_count === 'number' ? row.download_count : null,
    params: typeof row.params === 'number' ? row.params : null,
  };
}

/** One bench target, with the role narrowed the way the wire's roles are. */
function targetFrom(target: BenchTarget | undefined): BenchTarget {
  if (target === undefined) {
    return { file: '', role: 'other', pinned: false };
  }
  return { ...target, role: narrow(target.role, BENCH_ROLES, 'other') };
}

/**
 * The read-aloud set, narrowed where it enters: a page that trusted this
 * shape would crash on a server older than it, drawing a page stuck at
 * "reading" with nothing saying why.
 */
function readAloudFrom(value: ReadAloudState | undefined): ReadAloudState {
  const empty: ReadAloudState = {
    recordings: [],
    recording: false,
    error: null,
    passage: '',
    terms: [],
  };
  if (value === undefined || !Array.isArray(value.recordings)) return empty;
  return {
    recordings: value.recordings.filter(
      (recording) => typeof recording.id === 'string' && typeof recording.at === 'string',
    ),
    recording: value.recording === true,
    error: typeof value.error === 'string' ? value.error : null,
    passage: typeof value.passage === 'string' ? value.passage : '',
    terms: Array.isArray(value.terms) ? value.terms.filter((t) => typeof t === 'string') : [],
  };
}

function benchFrom(bench: BenchState | undefined): BenchState {
  const state: string | undefined = bench?.state;
  if (typeof state !== 'string' || !(BENCH_STATES as readonly string[]).includes(state)) {
    return { state: 'unknown' };
  }
  const known = bench as BenchState;
  if (known.state === 'running') {
    return {
      ...known,
      target: targetFrom(known.target),
      tier: narrow(known.tier, BENCH_TIERS, 'other'),
    };
  }
  if (known.state === 'failed') {
    return { ...known, target: targetFrom(known.target) };
  }
  return known;
}

function checkFrom(check: CatalogueCheck): CatalogueCheck {
  const state: string = check.state;
  return (CHECK_STATES as readonly string[]).includes(state) ? check : { state: 'unknown' };
}

function fromFrom(from: ActiveFrom | undefined): ActiveFrom {
  const source: string | undefined = from?.from;
  return typeof source === 'string' && (FROM_SOURCES as readonly string[]).includes(source)
    ? (from as ActiveFrom)
    : { from: 'unknown' };
}

/**
 * One tagged state, kept when its tag is one this client knows.
 *
 * A missing value is a server that predates the field - it narrows the same
 * way an unknown tag does, because drawing `idle` would claim the work this
 * field exists to report is not happening.
 */
function tagged<T extends { state: string }>(
  value: T | undefined,
  states: readonly string[],
): T | { state: 'unknown' } {
  if (value === undefined || typeof value.state !== 'string') return { state: 'unknown' };
  return states.includes(value.state) ? value : { state: 'unknown' };
}

function activateFrom(activate: ActivateState | undefined): ActivateState {
  const state: string | undefined = activate?.state;
  if (typeof state !== 'string' || !(ACTIVATE_STATES as readonly string[]).includes(state)) {
    return { state: 'unknown' };
  }
  const known = activate as ActivateState;
  if (known.state === 'activating' || known.state === 'failed') {
    return { ...known, role: narrow(known.role, ROLES, 'other') };
  }
  return known;
}
