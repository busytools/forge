/**
 * The schedule each fired prompt came from, keyed by the prompt's own id.
 *
 * **A delivery's prose carries the prompt and not the schedule**: the turn a
 * cron fire draws as is `[Cron]\n\n<prompt>`, so a row titled from it can
 * only say the prompt's first line. The `CronPromptAppended` frame the same
 * delivery arrives with names the schedule, and the display turn carries the
 * prompt's uuid - so the row joins the two here and reads `Morning summary`
 * where it would have read the prompt.
 *
 * **Both halves of the pairing land here.** The frame names the schedule while
 * the delivery arrives, and the conversation read carries the same pairing for
 * the rows it serves - so a page that reloads later titles the row as the live
 * frame did, rather than falling back to the prompt's own first line.
 */

import { SvelteMap } from 'svelte/reactivity';

class CronNames {
  #byUuid = new SvelteMap<string, string>();

  /**
   * Take the schedule a delivery named, from the frame that delivered it.
   *
   * A frame with no description - a cron registered without one - records
   * nothing: the row falls back to the prompt rather than to an empty title.
   */
  remember(uuid: unknown, description: unknown): void {
    if (typeof uuid !== 'string' || uuid === '') return;
    if (typeof description !== 'string' || description.trim() === '') return;
    this.#byUuid.set(uuid, description);
  }

  /** The schedule a delivered prompt came from, or `null` for one never seen. */
  nameFor(uuid: unknown): string | null {
    return typeof uuid === 'string' ? (this.#byUuid.get(uuid) ?? null) : null;
  }

  /**
   * Take the pairings a conversation read carried, keyed by the prompt's id.
   *
   * **The read half of the same pairing.** The frame names the schedule once,
   * while the delivery lands; a page that reloads afterwards reads the row
   * from the transcript, and this map is what lets it title the row as the
   * frame did. A description the schedule never had records nothing, exactly
   * as the frame's own `remember` treats it.
   */
  seed(names: unknown): void {
    if (typeof names !== 'object' || names === null || Array.isArray(names)) return;
    for (const [uuid, held] of Object.entries(names)) {
      const description =
        typeof held === 'object' && held !== null
          ? (held as Record<string, unknown>)['description']
          : null;
      this.remember(uuid, description);
    }
  }
}

export const cronNames = new CronNames();
