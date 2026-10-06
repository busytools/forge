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

/** One model forge runs: the pin, its live state, and the feed's join. */
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
}

/** One catalogue entry, as the candidate rows draw it. */
export interface CatalogueRow {
  variant: string;
  display_name: string;
  family: string;
  params: number;
  license: string | null;
  languages: string[];
  streaming: boolean;
  download: DownloadFact | null;
  speed: SpeedFact | null;
  wer: WerFact | null;
}

/** An update worth adopting for one in-service model. */
export interface ModelUpdate {
  role: ModelRole;
  file: string;
  current: { speed_x: number; fleurs_en_wer: number };
  candidate: CatalogueRow;
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
}

/** The roles the core names. */
const ROLES: Exclude<ModelRole, 'other'>[] = ['transcribing', 'normalization'];

/** The check states the server writes; the fifth is this client's own. */
const CHECK_STATES = ['never', 'checking', 'fresh', 'unreachable'] as const;

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
    in_use: data.in_use.map((model) => ({
      ...model,
      role: narrow(model.role, ROLES, 'other'),
      state: modelStateFrom(model.state),
    })),
    check: checkFrom(data.check),
    updates: data.updates.map((update) => ({
      ...update,
      role: narrow(update.role, ROLES, 'other'),
    })),
  };
}

function checkFrom(check: CatalogueCheck): CatalogueCheck {
  const state: string = check.state;
  return (CHECK_STATES as readonly string[]).includes(state) ? check : { state: 'unknown' };
}
