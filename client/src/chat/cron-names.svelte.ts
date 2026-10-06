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
 * **Held as the frames land, and not rebuilt from a read**: nothing on the
 * wire pairs the id with the name once the delivery is over, so a page that
 * reloads later falls back to the prompt's own first line. That fallback is
 * the row's whole title before this store exists, so nothing is lost that
 * was not already missing.
 */

import { SvelteMap } from 'svelte/reactivity';

export class CronNames {
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
}

export const cronNames = new CronNames();
