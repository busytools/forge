<script lang="ts">
  /**
   * A resolved thread, collapsed to one line - **one marker for every
   * surface that draws a thread** (Ved, 2026-10-17: "git diff also does not
   * show the resolve expansion like the file view does... I want all of them
   * to have almost similar things"). A press opens the thread again, which
   * is where its reopen and collapse acts live.
   */
  let {
    first,
    where,
    onopen,
  }: {
    /** The thread's first words, what the line reads by. */
    first: string;
    /** Where the thread sits: `path · line`. */
    where: string;
    onopen: () => void;
  } = $props();
</script>

<button class="rv-marker" type="button" onclick={onopen}>
  <span class="rv-mk resolved" aria-hidden="true"></span>
  <span class="rv-mk-word">resolved</span>
  <span class="rv-mk-first">{first}</span>
  <span class="rv-where">{where}</span>
</button>
