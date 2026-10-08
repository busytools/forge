/**
 * The seat's working tree, as the strip's row draws it.
 *
 * Held as the record is: one seat is on screen, so one row, replaced whole
 * when the record moves. A branch is a fact about the seat and not the
 * project, so the row reads the seat's own record and never the home.
 */
import type { GitStats, GitStrip } from '../session/view';

/** One stats block, copied off the record: the rows are arrays the view mutates never. */
const statsOf = (stats: GitStats | null): GitStats | null =>
  stats === null ? null : { ...stats, files: [...stats.files] };

export class Git {
  #strip = $state<GitStrip | null>(null);

  /** Take the record's read whole: what the wire says is what the row shows. */
  sync(strip: GitStrip | null): void {
    this.#strip =
      strip === null
        ? null
        : {
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
}

/** The one seat's tree, as its record last stated it. */
export const git = new Git();
