<script lang="ts">
  import { untrack } from 'svelte';

  import { beingWritten, type Turn as HeldTurn } from './conversation';
  import Strip from './Strip.svelte';
  import { fold, type Self, type TurnInfo } from './units';

  /**
   * The strip pinned above the box while the newest turn is being written.
   *
   * **It is a row of its own, and neither the composer nor the dock owns it**:
   * the client submits to the CLI and the CLI queues, so nothing else on the
   * page has a "prompt waiting" state of its own to hang this off. The row is
   * pinned for exactly as long as the turn is incomplete, and the turn's own
   * copy of it goes back into the turn when it settles.
   *
   * **The reader watches it become the finished strip rather than watching it
   * go.** At the turn's end the row shows the check and the cumulative for one
   * beat - the app's own held beat, the same length the composer gives a
   * landed take - and then detaches, leaving the settled row where a finished
   * turn's strip already draws.
   */
  const BEAT_MS = 450;

  let {
    turn,
    cwd = null,
    slot = null,
  }: {
    /** The newest turn, or `null` for a conversation that has none. */
    turn: HeldTurn | null;
    cwd?: string | null;
    slot?: Self | null;
  } = $props();

  /** The same fact the turn's own row folds under, so the two cannot disagree about the turn. */
  const writing = $derived(turn !== null && beingWritten(turn));

  /**
   * The turn's own row, folded the way the turn folds it.
   *
   * A `report` unit is the only source of these figures, and reusing the fold
   * is what keeps the pinned row and the row it settles into from disagreeing
   * about what the turn has done so far.
   */
  const info = $derived.by((): TurnInfo | null => {
    if (turn === null) return null;
    const units = fold(turn.messages, cwd, slot, writing);
    for (let at = units.length - 1; at >= 0; at -= 1) {
      const unit = units[at];
      if (unit !== undefined && unit.kind === 'report') return unit.info;
    }
    return null;
  });

  /** The turn whose row this pin is carrying, kept while its finished row beats. */
  let carried: string | null = $state(null);
  /** Whether the finished row is showing its beat. */
  let beating = $state(false);
  /** The beat's own timer, held rather than returned as a cleanup: a frame arriving mid-beat re-runs this effect. */
  let timer: ReturnType<typeof setTimeout> | null = null;

  function stop(): void {
    if (timer !== null) {
      clearTimeout(timer);
      timer = null;
    }
  }

  // The pin follows the turn: a running turn takes the row, and the turn that
  // finished under this pin keeps it for one beat.
  //
  // **The effect re-runs on every frame**, so what it reads of its own state it
  // reads untracked - a tracked read of `carried` would make its own write a
  // reason to run again, and the beat a reason to re-arm itself forever.
  $effect(() => {
    const which = turn === null ? null : turn.key;
    const running = info !== null && info.running;
    if (which === null || info === null) return;
    if (running) {
      // A turn being written takes the row back, whichever turn was beating
      // under it - the next turn can start inside the last one's beat.
      if (untrack(() => carried) !== which || untrack(() => beating)) {
        carried = which;
        beating = false;
        stop();
      }
      return;
    }
    // A row that arrived finished is the turn's own, not this pin's: only the
    // turn this pin watched run is carried over, and only once.
    if (untrack(() => carried) !== which || untrack(() => beating)) return;
    carried = which;
    beating = true;
    stop();
    timer = setTimeout(() => {
      beating = false;
      carried = null;
      timer = null;
    }, BEAT_MS);
  });

  // The timer goes with the column, which is the one thing the effect above
  // cannot do for itself.
  $effect(() => () => stop());

  /**
   * What the row draws.
   *
   * The key is checked rather than trusted: a beat that outlived its turn -
   * an occupant swapped under the page - must not carry the NEXT turn's row
   * with it.
   */
  const shown = $derived.by((): TurnInfo | null => {
    if (info === null || turn === null) return null;
    if (info.running) return info;
    return beating && carried === turn.key ? info : null;
  });
</script>

{#if shown !== null}
  <div class="strip">
    <div class="ti"><Strip info={shown} /></div>
  </div>
{/if}
