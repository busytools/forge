<script lang="ts">
  import type { Readable } from 'svelte/store';

  import type { Connection } from '../../socket';
  import type { SessionSlot } from '../../wire/types';
  import Chat from '../Chat.svelte';

  /**
   * The session page's own shape, for a column that has to survive it.
   *
   * **The page re-derives what it hands the column on every read it makes**,
   * and hands it over through a spread: a seat re-reads on every frame it
   * emits, so the object is new many times a second while its words arrive.
   * `reads` stands in for those reads - the page's own count, and no prop the
   * column declares.
   */
  let {
    slot,
    connection,
    reads,
  }: {
    slot: SessionSlot;
    connection: Connection;
    reads: Readable<number>;
  } = $props();

  const handed = $derived({
    slot,
    connection,
    waking: false,
    reason: null,
    read: $reads,
  });
</script>

<Chat {...handed} />
