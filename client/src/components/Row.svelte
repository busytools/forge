<script lang="ts">
  import {
    artifactLabel,
    asleep,
    followable,
    markOf,
    waitingOn,
    whenOf,
    type Row,
  } from '../home/view';
  import { hrefForSlot } from '../routes';
  import Mark from './Mark.svelte';

  /**
   * One row. A lead and a worker share every column, and the state arrives
   * already decided - nothing here recomputes it.
   */
  let { row, now, refused = null }: { row: Row; now: number; refused?: string | null } = $props();

  const mark = $derived(markOf(row.state));
  const href = $derived(row.task?.artifact ? followable(row.task.artifact) : null);
</script>

<div class="row {mark.class}">
  <Mark state={row.state} />
  <!-- The name is the link rather than the row: a row can carry an artifact
       anchor, and an anchor inside an anchor is not HTML. A sleeping seat is
       not a link at all - its page draws a refusal - so the row is read as
       information, which is the terminal's own treatment of one. -->
  <span class="name">
    {#if asleep(row.state)}
      {row.name}
    {:else}
      <a href={hrefForSlot(row.slot)}>{row.name}</a>
    {/if}
  </span>
  <span class="where">
    {#if row.place.branch}{row.place.branch}{/if}
    {#if row.place.branch && row.place.files}{' \u{b7} '}{/if}
    {#if row.place.files}<span class="files">{row.place.files}</span>{/if}
  </span>
  <span class="what">
    {#if row.pending}
      <span class="txt">{waitingOn(row.pending)}</span>
    {:else if row.task}
      <span class="txt">{row.task.subject}</span>
      <span class="st">{row.task.chip}</span>
      {#if row.task.artifact && href}
        <a {href} target="_blank" rel="noreferrer">{artifactLabel(row.task.artifact)}</a>
      {:else if row.task.artifact}
        {artifactLabel(row.task.artifact)}
      {/if}
    {:else if refused}
      <span class="txt">{refused}</span>
    {:else if row.gate}
      <span class="txt">{row.gate}</span>
    {:else}
      <span class="txt">{'\u{b7}'}</span>
    {/if}
  </span>
  <span class="when">{whenOf(row, now)}</span>
</div>

{#if row.reason}
  <div class="note">{row.reason}</div>
{/if}
