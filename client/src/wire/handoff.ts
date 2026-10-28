/**
 * The handoff record, narrowed where it enters the client.
 *
 * It arrives two ways - as a live `handoff` update and inside a seat
 * record's conversation - and both are the same shape, so they narrow here
 * rather than in each reader.
 */

/** One labelled block of the brief. */
export interface HandoffSection {
  label: string;
  lines: string[];
}

/** The cycle a handoff ended: the handover's own counts. */
export interface HandoffCycle {
  requests: number;
  turns: number;
  prompt_tokens: number;
}

/** How the successor's first turns went; absent until they have been measured. */
export interface HandoffSuccessor {
  turns: number;
  reread_files: number;
  questions: number;
  corrections: number;
}

export interface HandoffRecord {
  from: string;
  to: string;
  /** `fill`, `compaction` or `manual`; anything else draws as itself. */
  reason: string;
  fill_percent: number | null;
  total_tokens: number | null;
  max_tokens: number | null;
  at_percent: number;
  /** When the swap happened, as the record's own RFC 3339 stamp. */
  at: string | null;
  cycle: HandoffCycle;
  successor: HandoffSuccessor | null;
  ground_truth: HandoffSection[];
  judgment: HandoffSection[];
  /**
   * The turn the handing-over session ran the procedure in, as the wire
   * carries its frames.
   *
   * **Raw frames, not narrowed.** The chat folds them the way it folds any
   * turn, and a record shape would be a second vocabulary for rows the fold
   * already reads.
   */
  judgment_turn: unknown[];
}

export function handoffOf(value: unknown): HandoffRecord | null {
  const held = record(value);
  const from = text(held['from']);
  const to = text(held['to']);
  if (from === null || to === null) return null;
  const brief = record(held['brief']);
  const cycle = record(held['cycle']);
  return {
    from,
    to,
    reason: text(held['reason']) ?? 'manual',
    fill_percent: number(held['fill_percent']),
    total_tokens: number(held['total_tokens']),
    max_tokens: number(held['max_tokens']),
    at_percent: number(held['at_percent']) ?? 0,
    at: text(held['at']),
    cycle: {
      requests: number(cycle['requests']) ?? 0,
      turns: number(cycle['turns']) ?? 0,
      prompt_tokens: number(cycle['prompt_tokens']) ?? 0,
    },
    successor: successorOf(held['successor']),
    ground_truth: sectionsOf(brief['ground_truth']),
    judgment: sectionsOf(brief['judgment']),
    judgment_turn: list(held['judgment_turn']),
  };
}

/** An unmeasured successor is absent, not a row of zeros. */
function successorOf(value: unknown): HandoffSuccessor | null {
  if (value === null || typeof value !== 'object' || Array.isArray(value)) return null;
  const held = record(value);
  return {
    turns: number(held['turns']) ?? 0,
    reread_files: number(held['reread_files']) ?? 0,
    questions: number(held['questions']) ?? 0,
    corrections: number(held['corrections']) ?? 0,
  };
}

/** The short form a row names a session by, the same eight characters the
 * server's own pair uses. */
export function shortId(id: string): string {
  return id.slice(0, 8);
}

/** The row's address, which is also its turn's key: a reconnect folds the
 * same record twice and must land on one row. */
export function handoffKey(handoff: HandoffRecord): string {
  return `handoff:${handoff.from}:${handoff.to}`;
}

/** The frame a conversation turn carries for one handoff.
 *
 * The record is already narrowed, and the fold reads a frame the way the wire
 * sends one, so the halves go back under `brief` here. */
export function handoffFrame(handoff: HandoffRecord): Record<string, unknown> {
  return {
    type: 'forge_handoff',
    handoff: {
      from: handoff.from,
      to: handoff.to,
      reason: handoff.reason,
      fill_percent: handoff.fill_percent,
      total_tokens: handoff.total_tokens,
      max_tokens: handoff.max_tokens,
      at_percent: handoff.at_percent,
      brief: { ground_truth: handoff.ground_truth, judgment: handoff.judgment },
      judgment_turn: handoff.judgment_turn,
    },
  };
}

function sectionsOf(value: unknown): HandoffSection[] {
  if (!Array.isArray(value)) return [];
  return value.flatMap((entry) => {
    const section = record(entry);
    const label = text(section['label']);
    if (label === null) return [];
    return [{ label, lines: list(section['lines']).map((line) => String(line)) }];
  });
}

function record(value: unknown): Record<string, unknown> {
  return value !== null && typeof value === 'object' ? (value as Record<string, unknown>) : {};
}

function list(value: unknown): unknown[] {
  return Array.isArray(value) ? value : [];
}

function text(value: unknown): string | null {
  return typeof value === 'string' ? value : null;
}

function number(value: unknown): number | null {
  return typeof value === 'number' && Number.isFinite(value) ? value : null;
}
