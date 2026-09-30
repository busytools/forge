<script lang="ts">
  /**
   * The composer over a record the caller can replace.
   *
   * A page hands the composer a whole new record on every frame the server
   * answers with, so the only way to measure what a frame costs it is to hand
   * it one - and the record is a prop, whose parent this stands in for.
   *
   * Test support: nothing the app ships imports it.
   */
  import { untrack } from 'svelte';

  import Composer from '../Composer.svelte';
  import type { ComposerRecord, SeatRead } from '../view';
  import type { Connection } from '../../socket';
  import type { SessionSlot } from '../../wire/types';

  let {
    initial,
    slot,
    connection,
    seat,
    dictation,
  }: {
    initial: ComposerRecord;
    slot: SessionSlot;
    connection: Pick<Connection, 'dispatch' | 'onMessage'>;
    seat: SeatRead;
    dictation: boolean;
  } = $props();

  let record = $state<ComposerRecord>(untrack(() => initial));

  /** One arriving frame's answer, which is a whole new record. */
  export function push(next: ComposerRecord): void {
    record = next;
  }
</script>

<Composer {record} {slot} {seat} {connection} {dictation} />
