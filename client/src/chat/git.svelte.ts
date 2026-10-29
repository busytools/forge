/**
 * The seat's working tree, as the strip's row draws it.
 *
 * Held as the record is: one seat is on screen, so one row, replaced whole
 * when the record moves. A branch is a fact about the seat and not the
 * project, so the row reads the seat's own record - the home contributes
 * one fact the record cannot carry, the NAME of a tree the worker runs in
 * that is not its project's, which the strip's row states.
 */
import type { GitStats, GitStrip } from '../session/view';

/** One stats block, copied off the record: the rows are arrays the view mutates never. */
const statsOf = (stats: GitStats | null): GitStats | null =>
  stats === null ? null : { ...stats, files: [...stats.files] };

/** How long an event stays on the strip before the row is the tree again. */
const NOTE_MS = 8_000;

/** A repository mutation, as the strip says it: the wire's kind in a reader's words. */
export interface StripNote {
  line: string;
}

/**
 * The CLI's own words for a mutation, from the same kinds the chat row used.
 * An unknown kind is "something changed", which is what the CLI asks consumers
 * to assume.
 */
export function vcsLine(kind: string | null, branch: string | null): string {
  const said =
    kind === 'commit'
      ? 'committed'
      : kind === 'push'
        ? 'pushed'
        : kind === 'merge'
          ? 'merged'
          : kind === 'rebase'
            ? 'rebased'
            : `repository changed${kind === null ? '' : ` (${kind})`}`;
  if (branch === null) return said;
  return kind === 'push' ? `${said} ${branch}` : `${said} on ${branch}`;
}

class Git {
  #strip = $state<GitStrip | null>(null);
  #note = $state<StripNote | null>(null);
  #timer: ReturnType<typeof setTimeout> | null = null;

  /** Take the record's read whole: what the wire says is what the row shows. */
  sync(strip: GitStrip | null): void {
    if (strip === null) {
      this.clearNote();
      this.#strip = null;
      return;
    }
    this.#strip = {
      ...strip,
      ahead:
        strip.ahead === null
          ? null
          : {
              ...strip.ahead,
              commits: strip.ahead.commits.map((commit) => ({
                ...commit,
                stats: statsOf(commit.stats),
              })),
              stats: statsOf(strip.ahead.stats),
            },
      uncommitted: statsOf(strip.uncommitted),
      pr: strip.pr === null ? null : { ...strip.pr },
    };
  }

  /** Whether there is a tree read to draw at all. */
  anything(): boolean {
    return this.#strip !== null;
  }

  strip(): GitStrip | null {
    return this.#strip;
  }

  /** Put a repository event on the strip; it clears itself. */
  note(kind: string | null, branch: string | null): void {
    this.#note = { line: vcsLine(kind, branch) };
    if (this.#timer !== null) clearTimeout(this.#timer);
    this.#timer = setTimeout(() => {
      this.#note = null;
      this.#timer = null;
    }, NOTE_MS);
  }

  noteNow(): StripNote | null {
    return this.#note;
  }

  /** Drop the event and the tree: a test's reset, and a seat with no record. */
  clear(): void {
    this.clearNote();
    this.#strip = null;
  }

  private clearNote(): void {
    if (this.#timer !== null) clearTimeout(this.#timer);
    this.#timer = null;
    this.#note = null;
  }
}

/** The one seat's tree, as its record last stated it. */
export const git = new Git();
