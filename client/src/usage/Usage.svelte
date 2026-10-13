<script lang="ts">
  /**
   * The usage route: the subject it watches, what the page draws before the
   * read lands, and the one action the page has.
   *
   * The subscription is this page's own and goes with it: only this page
   * draws the report, and a forge that nobody is reading must not keep a
   * subscription that would scan its pool on the next ask.
   */

  import type { Connection } from '../socket';
  import { watchUsage, type UsageRead } from './live';
  import UsageBody from './UsageBody.svelte';
  import { isEmpty } from './view';

  let { connection, mark = null }: { connection: Connection | null; mark?: string | null } =
    $props();

  let read = $state<UsageRead>({ wire: null, refused: null });

  $effect(() => {
    const open = connection;
    if (open === null) return;
    return watchUsage(open).subscribe((value) => {
      read = value;
    });
  });

  /**
   * Ask the subject again. The scan is the answer rather than an update -
   * the subject hears nothing after its first one - so this is the only way
   * fresh figures arrive, and the same snapshot path draws them.
   */
  function refresh(): void {
    connection?.refresh('usage');
  }
</script>

{#if read.refused !== null}
  <!-- The server turned the subscription down, in its own words. The address
       is not what is wrong, so the door is not what is drawn. -->
  <main class="wrap usage">
    <p class="pending">This forge would not answer for the usage: {read.refused}</p>
  </main>
{:else if read.wire === null}
  <main class="wrap usage"><p class="pending">Reading the usage...</p></main>
{:else if isEmpty(read.wire)}
  <!-- A pool with nothing in it is its own page: a headline of zeros over an
       empty table reads as a report that failed, not one with nothing yet. -->
  <main class="wrap usage">
    <p class="pending">No usage yet: this forge's session transcripts hold no tokens.</p>
  </main>
{:else}
  <UsageBody wire={read.wire} {mark} onrefresh={refresh} />
{/if}
