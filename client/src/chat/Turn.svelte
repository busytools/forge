<script lang="ts">
  import Icon from '../components/Icon.svelte';
  import Card from './Card.svelte';
  import Compacting from './Compacting.svelte';
  import type { Turn as HeldTurn } from './conversation';
  import Group from './Group.svelte';
  import Hooks from './Hooks.svelte';
  import Messages from './Messages.svelte';
  import Notice from './Notice.svelte';
  import { bytes } from './numbers';
  import Prose from './Prose.svelte';
  import Report from './Report.svelte';
  import Thinking from './Thinking.svelte';
  import { fold, type Self, type Unit } from './units';

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
    slot = null,
    compacting = false,
  }: { turn: HeldTurn; cwd: string | null; slot?: Self | null; compacting?: boolean } = $props();

  // `turn.live` is the caller's fact the fold cannot read off the frames: a
  // saved page carries no result frame either. It is what a running row
  // needs.
  const units = $derived(fold(turn.messages, cwd, slot, turn.live));

  /** A reader's own words on their own, or a run of everything else in one block. */
  type Block =
    | { mine: true; unit: Extract<Unit, { kind: 'user' }> }
    | { mine: false; units: Unit[]; footer: number; trailing: boolean };

  const layout = $derived.by(() => {
    const out: Block[] = [];
    for (const unit of units) {
      if (unit.kind === 'user') {
        out.push({ mine: true, unit });
        continue;
      }
      const last = out[out.length - 1];
      if (last !== undefined && !last.mine) last.units.push(unit);
      else out.push({ mine: false, units: [unit], footer: 0, trailing: false });
    }
    // Where the turn's footer begins: the hooks chip and the report row, which
    // the compaction line sits ABOVE - the order the terminal settled, and the
    // one the book's page and the approved mockup both draw.
    return out.map((block) => {
      if (block.mine) return block;
      const footer = footerOf(block.units);
      return { ...block, footer, trailing: footer === block.units.length };
    });
  });

  /** The index the trailing hooks and report units start at. */
  function footerOf(units: readonly Unit[]): number {
    let at = units.length;
    while (at > 0) {
      const kind = units[at - 1]?.kind;
      if (kind !== 'hooks' && kind !== 'report') break;
      at -= 1;
    }
    return at;
  }

  /**
   * Whether the compaction line needs a block of its own.
   *
   * A block of its own is for a turn that ends on the reader's own words,
   * where the line would otherwise sit inside their attribution.
   */
  const loose = $derived(compacting && layout.at(-1)?.mine !== false);
</script>

{#each layout as block, at (at)}
  {#if block.mine}
    <!-- No label: the orange rule is the attribution. -->
    <div class="mine">
      <Prose text={block.unit.text} preserveLines />
      {#each block.unit.files as file, index (`att-${index}`)}
        <div class="attrow">
          <Icon name="read" class="gl" />
          <span>{file.mime ?? file.kind}</span>
          {#if file.bytes !== null}<span class="n">{bytes(file.bytes)}</span>{/if}
        </div>
      {/each}
    </div>
  {:else}
    <div class="work">
      {#each block.units as unit, index (unit.key)}
        {#if compacting && at === layout.length - 1 && index === block.footer}
          <Compacting />
        {/if}
        {#if unit.kind === 'text'}
          <Prose text={unit.text} />
        {:else if unit.kind === 'thinking'}
          <Thinking text={unit.text} />
        {:else if unit.kind === 'group'}
          <Group families={unit.families} status={unit.status} />
        {:else if unit.kind === 'question'}
          <Card asked={unit.asked} />
        {:else if unit.kind === 'messages'}
          <Messages lanes={unit.lanes} status={unit.status} />
        {:else if unit.kind === 'notice'}
          <Notice notice={unit.notice} />
        {:else if unit.kind === 'hooks'}
          <Hooks actions={unit.actions} infos={unit.infos} />
        {:else if unit.kind === 'report'}
          <Report info={unit.info} />
        {/if}
      {/each}
      {#if compacting && at === layout.length - 1 && block.trailing}
        <Compacting />
      {/if}
    </div>
  {/if}
{/each}
{#if loose}
  <div class="work"><Compacting /></div>
{/if}
