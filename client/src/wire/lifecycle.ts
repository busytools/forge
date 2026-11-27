/**
 * One prompt's place in its life, narrowed once where it enters the client.
 *
 * **The whole page reads these three shapes, not the wire's spellings.** A
 * refusal carries its reason on the state rather than in a field beside it, so
 * an item's words and its reason cannot be paired wrongly; a shape this build
 * does not name stays `unknown`, which every caller draws as still waiting -
 * a state the server adds later must leave a row visible, never settle it on a
 * parse miss (rule 25).
 *
 * `sending` is deliberately absent: it never crosses the wire. It is this
 * client's own state for a press the core has not answered yet.
 */
export type PromptState =
  | { kind: 'queued' }
  | { kind: 'sent' }
  | { kind: 'not_sent'; reason: string }
  | { kind: 'unknown' };

/** How the core learned a prompt's state, as the row's detail reads it. */
export type LifecycleOrigin =
  /** The CLI's own frame said it. */
  | { kind: 'cli' }
  /** The core drew it because no CLI frame was coming; `note` says so. */
  | { kind: 'forge'; note: string }
  /** A shape this build does not name. */
  | { kind: 'unknown' };

function record(value: unknown): Record<string, unknown> | null {
  return value !== null && typeof value === 'object' ? (value as Record<string, unknown>) : null;
}

/** The wire's state value, narrowed. */
export function promptStateFrom(value: unknown): PromptState {
  if (value === 'queued') return { kind: 'queued' };
  if (value === 'sent') return { kind: 'sent' };
  const outer = record(value);
  const refusal = record(outer?.['not_sent']);
  if (refusal !== null) {
    const reason = refusal['reason'];
    return { kind: 'not_sent', reason: typeof reason === 'string' ? reason : '' };
  }
  return { kind: 'unknown' };
}

/** The wire's source value, narrowed. */
export function lifecycleOriginFrom(value: unknown): LifecycleOrigin {
  if (value === 'cli') return { kind: 'cli' };
  const outer = record(value);
  const forge = record(outer?.['forge']);
  if (forge !== null) {
    const note = forge['note'];
    return { kind: 'forge', note: typeof note === 'string' ? note : '' };
  }
  return { kind: 'unknown' };
}
