<script lang="ts">
  import Icon from '../components/Icon.svelte';
  import Card from './Card.svelte';
  import CompactionPoint from './CompactionPoint.svelte';
  import { beingWritten, type Turn as HeldTurn } from './conversation';
  import Group from './Group.svelte';
  import Hooks from './Hooks.svelte';
  import Notice from './Notice.svelte';
  import { bytes } from './numbers';
  import Prose from './Prose.svelte';
  import Report from './Report.svelte';
  import Skill from './Skill.svelte';
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
   * messages are - one group per stretch of work, a lane per family and per
   * kind of peer traffic, a card for a question, a notice for a delivery - and
   * this draws what it is given.
   */
  let {
    turn,
    slot = null,
    carried = null,
  }: {
    turn: HeldTurn;
    slot?: Self | null;
    /**
     * The key of this turn's report row that the pin above the box is drawing,
     * or `null` where the pin is drawing none of them.
     *
     * **The row rather than a yes**, so the turn stands aside for exactly what
     * the pin holds and nothing else: a turn can carry more than one report
     * row - one per Result that landed in it - and the pin takes one of them.
     * Dropping every report row here would lose the rows the pin never took,
     * and dropping none would draw the one it holds twice.
     */
    carried?: string | null;
  } = $props();

  const folded = $derived(fold(turn.messages, slot, beingWritten(turn)));

  /**
   * The fold's units, less the one row the pin is carrying.
   *
   * The row moves out of the turn and above the composer while the pin holds
   * it - the running row while the turn is written, and the finished one for
   * the beat after it ends - so it is not drawn here as well. Nothing is lost
   * by that: the same row is on the page, in the pin, and the turn takes it
   * back the moment the pin lets go.
   */
  const units = $derived(carried === null ? folded : folded.filter((unit) => unit.key !== carried));

  /** A reader's own words on their own, or a run of everything else in one block. */
  type Block =
    | { mine: true; unit: Extract<Unit, { kind: 'user' }> }
    | { mine: false; nth: number; units: Unit[] };

  /**
   * What identifies a block: a reader's own words by their own key, a run of
   * everything else by which run of that kind it is.
   *
   * **Keyed this way, a block inserted above content already drawn is MOVED
   * rather than remade**, and a block is where a call's open state lives, so
   * remaking it closes what the reader had open. Two insertions make that case,
   * and they need different stability:
   *
   * - A page landing with its copy of a live turn beginning at the reader's own
   *   words puts a `mine` block at the FRONT, so every position below shifts.
   *   Counting the work runs among themselves is what survives that, and the
   *   `mine` blocks carry their own keys either way.
   * - A thought landing above a call inserts a unit at the HEAD of a run, so
   *   the run's first unit is not a stable name for it. Which run of its kind
   *   it is does not move.
   *
   * The units inside a block were already keyed by their own keys; the blocks
   * themselves were keyed by position, which is neither of these.
   */
  function blockKey(block: Block): string {
    return block.mine ? block.unit.key : `work-${block.nth}`;
  }

  const layout = $derived.by(() => {
    const out: Block[] = [];
    for (const unit of units) {
      if (unit.kind === 'user') {
        out.push({ mine: true, unit });
        continue;
      }
      const last = out[out.length - 1];
      if (last !== undefined && !last.mine) last.units.push(unit);
      else out.push({ mine: false, nth: 0, units: [unit] });
    }
    return out.map((block, nth) => {
      if (block.mine) return block;
      // Which run of its kind this is, which is what it is keyed by: a run's
      // own units can grow at either end, so neither end names it.
      const runs = out.slice(0, nth).filter((before) => !before.mine).length;
      return { ...block, nth: runs };
    });
  });
</script>

{#each layout as block (blockKey(block))}
  {#if block.mine}
    <!-- No label: the orange rule is the attribution. -->
    <div class="mine" data-k={`${turn.key}:${blockKey(block)}`}>
      <Prose text={block.unit.text} preserveLines />
      {#each block.unit.files as file, index (`att-${index}`)}
        <div class="attrow">
          <Icon name="read" class="gl" />
          <span>{file.mime ?? file.kind}</span>
          {#if file.bytes !== null}<span class="n">{bytes(file.bytes)}</span>{/if}
        </div>
      {/each}
      {#if block.unit.note !== undefined}
        <!-- The drained prompt's own wait, written where the client released
             the row - the pile's vocabulary, kept as the row's record of it. -->
        <div class="st">{block.unit.note}</div>
      {/if}
    </div>
  {:else}
    <div class="work">
      {#each block.units as unit (unit.key)}
        <!-- The unit's own key, in the DOM, for the one reader that has to
             find the row again after the layout moves: the column's anchor
             (`chat/anchor.ts`). **Prefixed with the turn's key** because the
             fold names an id-less frame within its own turn (`f2`), so two
             turns can carry the same unit key and an unprefixed lookup could
             hold a row of the wrong one. A plain block, so nothing the sheet
             draws changes. -->
        <div class="unit" data-k={`${turn.key}:${unit.key}`}>
          {#if unit.kind === 'text'}
            <Prose text={unit.text} />
          {:else if unit.kind === 'group'}
            <Group lanes={unit.lanes} />
          {:else if unit.kind === 'question'}
            <Card asked={unit.asked} />
          {:else if unit.kind === 'notice'}
            <Notice notice={unit.notice} />
          {:else if unit.kind === 'skill'}
            <Skill name={unit.name} body={unit.body} />
          {:else if unit.kind === 'hooks'}
            <Hooks actions={unit.actions} infos={unit.infos} errors={unit.errors} />
          {:else if unit.kind === 'compaction'}
            <CompactionPoint
              trigger={unit.trigger}
              preTokens={unit.preTokens}
              postTokens={unit.postTokens}
              summary={unit.summary}
            />
          {:else if unit.kind === 'report'}
            <Report info={unit.info} />
          {/if}
        </div>
      {/each}
    </div>
  {/if}
{/each}
