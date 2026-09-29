<script lang="ts">
  import Call from './Call.svelte';
  import type { Turn } from './conversation';
  import Prose from './Prose.svelte';
  import { rowsOf, type ProseRow, type Row } from './rows';

  /**
   * One turn of the conversation, as the page draws it: what the reader said,
   * then everything the assistant did about it.
   *
   * **The turn is a ROW of the virtualised list**, and its key comes from the
   * conversation rather than from where it sits, so a turn prepended above it
   * neither re-measures it nor closes what the reader has open.
   *
   * **It does not group.** A run of consecutive calls is drawn as the calls it
   * is, one row each, in the order the turn wrote them: the group, its family
   * lanes and its roll-up are the fold's, and they arrive on top of this.
   */
  let { turn, cwd }: { turn: Turn; cwd: string | null } = $props();

  const rows = $derived(rowsOf(turn, cwd));

  /**
   * A reader's own words on their own, or a run of everything else in one
   * block. The block is what carries the padding between them, so a turn's
   * rows sit together rather than each taking the space of a turn.
   */
  type Block = { mine: true; row: ProseRow } | { mine: false; rows: Row[] };

  const layout = $derived.by(() => {
    const out: Block[] = [];
    for (const row of rows) {
      if (row.kind === 'prose' && row.mine) {
        out.push({ mine: true, row });
        continue;
      }
      const last = out[out.length - 1];
      if (last !== undefined && !last.mine) last.rows.push(row);
      else out.push({ mine: false, rows: [row] });
    }
    return out;
  });
</script>

{#each layout as block, at (at)}
  {#if block.mine}
    <!-- No label: the orange rule is the attribution. -->
    <div class="mine">{block.row.text}</div>
  {:else}
    <div class="work">
      {#each block.rows as row (row.key)}
        {#if row.kind === 'call'}
          <Call call={row} />
        {:else}
          <Prose text={row.text} />
        {/if}
      {/each}
    </div>
  {/if}
{/each}
