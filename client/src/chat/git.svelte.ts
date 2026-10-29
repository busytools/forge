/**
 * The seat's working tree, as the strip's row draws it.
 *
 * Held as the record is: one seat is on screen, so one row, replaced whole
 * when the record moves. A branch is a fact about the seat and not the
 * project, so the row reads the seat's own record - the home contributes
 * one fact the record cannot carry, the NAME of a tree the worker runs in
 * that is not its project's, which the strip's row states.
 */
import type { SessionSlot } from '../wire/types';
import type { GitStats, GitStrip } from '../session/view';

/** The one shown seat's address, which is what the strip belongs to. */
const seatKey = (slot: SessionSlot): string =>
  `${slot.org}\u0000${slot.project}\u0000${slot.label}`;

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

/**
 * The strip's line for a published change: the action the CLI announced, on
 * the change it names. Every action gets one - the four that keep a chat row
 * and the ones that do not alike - so a comment, an edit or a checkout is not
 * silent (rule 25).
 */
export function changeLine(
  provider: string | null,
  identifier: string | null,
  action: string | null,
  repo: string | null,
): string {
  const noun =
    provider === 'gitlab'
      ? 'MR'
      : provider?.startsWith('github') === true || provider === 'bitbucket'
        ? 'PR'
        : 'change';
  const named = identifier === null || identifier === '' ? noun : `${noun} #${identifier}`;
  const did = changeVerb(action);
  return repo === null || repo === '' ? `${named} ${did}` : `${named} ${did} in ${repo}`;
}

/** The CLI's action in the strip's own words; an unknown one as the wire's. */
function changeVerb(action: string | null): string {
  switch (action) {
    case null:
    case '':
      return 'changed';
    case 'checked-out':
      return 'checked out';
    case 'auto-merge-enabled':
      return 'auto-merge on';
    case 'auto-merge-disabled':
      return 'auto-merge off';
    case 'ready':
      return 'marked ready';
    case 'draft':
      return 'marked draft';
    default:
      return action;
  }
}

class Git {
  #strip = $state<GitStrip | null>(null);
  #note = $state<StripNote | null>(null);
  #timer: ReturnType<typeof setTimeout> | null = null;
  /** The seat the strip is on screen for; a note from any other seat is not its. */
  #shown: string | null = null;

  /** Name the seat the strip is drawing; a different seat drops its event. */
  show(slot: SessionSlot | null): void {
    const key = slot === null ? null : seatKey(slot);
    if (this.#shown !== key) this.clearNote();
    this.#shown = key;
  }

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

  /**
   * Put a repository event on the strip, but only for the seat it belongs to.
   *
   * **A visited seat keeps folding in the background**, so a worker's commit
   * would otherwise land on the lead's strip beside the lead's branch - the
   * note is the SHOWN seat's, and an away seat's event is not this row's.
   */
  note(slot: SessionSlot, line: string): void {
    if (this.#shown === null || this.#shown !== seatKey(slot)) return;
    this.#note = { line };
    if (this.#timer !== null) clearTimeout(this.#timer);
    this.#timer = setTimeout(() => {
      this.#note = null;
      this.#timer = null;
    }, NOTE_MS);
  }

  noteNow(): StripNote | null {
    return this.#note;
  }

  private clearNote(): void {
    if (this.#timer !== null) clearTimeout(this.#timer);
    this.#timer = null;
    this.#note = null;
  }
}

/** The one seat's tree, as its record last stated it. */
export const git = new Git();
