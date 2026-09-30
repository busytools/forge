<script lang="ts">
  import Icon from '../components/Icon.svelte';
  import Card from './Card.svelte';
  import type { Turn as HeldTurn } from './conversation';
  import Group from './Group.svelte';
  import Hooks from './Hooks.svelte';
  import Notice from './Notice.svelte';
  import { bytes } from './numbers';
  import Prose from './Prose.svelte';
  import Report from './Report.svelte';
  import { fold, type Unit } from './units';

  /**
   * One turn of the conversation, as the page draws it: what the reader said,
   * then everything the assistant did about it.
   *
   * **The turn is a ROW of the virtualised list**, and its key comes from the
   * conversation rather than from where it sits, so a turn prepended above it
   * neither re-measures it nor closes what the reader has open.
   *
   * **The fold is not this component's.** `fold` decides what the turn's
   * messages are - one group per run of calls, one lane per family, a card for
   * a question, a notice for a delivery - and this draws what it is given.
   */
  let {
    turn,
    cwd,
    compacting = false,
  }: { turn: HeldTurn; cwd: string | null; compacting?: boolean } = $props();

  const units = $derived(fold(turn.messages, cwd));

  /** A reader's own words on their own, or a run of everything else in one block. */
  type Block =
    { mine: true; unit: Extract<Unit, { kind: 'user' }> } | { mine: false; units: Unit[] };

  const layout = $derived.by(() => {
    const out: Block[] = [];
    for (const unit of units) {
      if (unit.kind === 'user') {
        out.push({ mine: true, unit });
        continue;
      }
      const last = out[out.length - 1];
      if (last !== undefined && !last.mine) last.units.push(unit);
      else out.push({ mine: false, units: [unit] });
    }
    return out;
  });

  /**
   * Whether the compaction line needs a block of its own.
   *
   * It is the last thing in the turn either way, and the turn's own last block
   * draws it where that block is the work: a block of its own is for a turn
   * that ends on the reader's own words, where the line would otherwise sit
   * inside their attribution.
   */
  const loose = $derived(compacting && layout.at(-1)?.mine !== false);
</script>

{#each layout as block, at (at)}
  {#if block.mine}
    <!-- No label: the orange rule is the attribution. -->
    <div class="mine">
      {block.unit.text}{#each block.unit.files as file, index (`att-${index}`)}<div class="attrow">
          <Icon name="read" class="gl" />
          <span>{file.mime ?? file.kind}</span>
          {#if file.bytes !== null}<span class="n">{bytes(file.bytes)}</span>{/if}
        </div>{/each}
    </div>
  {:else}
    <div class="work">
      {#each block.units as unit, index (`${at}-${index}`)}
        {#if unit.kind === 'text'}
          <Prose text={unit.text} />
        {:else if unit.kind === 'group'}
          <Group families={unit.families} status={unit.status} />
        {:else if unit.kind === 'question'}
          <Card kind="question" asked={unit.asked} />
        {:else if unit.kind === 'peer'}
          <Card kind="peer" card={unit.card} />
        {:else if unit.kind === 'peers'}
          <Card kind="peers" cards={unit.cards} />
        {:else if unit.kind === 'notice'}
          <Notice notice={unit.notice} />
        {:else if unit.kind === 'hooks'}
          <Hooks actions={unit.actions} infos={unit.infos} />
        {:else if unit.kind === 'report'}
          <Report info={unit.info} />
        {/if}
      {/each}
      {#if compacting && at === layout.length - 1}
        <div class="compacting"><span class="ring"></span>Compacting context...</div>
      {/if}
    </div>
  {/if}
{/each}
{#if loose}
  <div class="work">
    <div class="compacting"><span class="ring"></span>Compacting context...</div>
  </div>
{/if}
