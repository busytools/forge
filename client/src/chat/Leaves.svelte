<script lang="ts">
  import Call from './Call.svelte';
  import Hook from './Hook.svelte';
  import Inbound from './Inbound.svelte';
  import { opensByDefault, type ToolLeaf } from './leaves';
  import PeerRow from './PeerRow.svelte';
  import Thought from './Thought.svelte';
  import type { WorkRow } from './units';

  /**
   * A stretch of the turn's work: every call, message and thought on its own
   * row behind a rail, in the order it arrived.
   *
   * **No grouping, and the list only appends.** A kind is not a row of its
   * own: the kind's glyph rides the row that draws it, and a row that takes
   * one never moves - so nothing rearranges under the reader and no header
   * sits above anything.
   *
   * **A message is a tool row, and so is a thought.** They carry no tint, no
   * border and no colour of their own - the same rows and marks a run of calls
   * draws - which is what makes traffic and reasoning read as the same system
   * rather than as a second one. The row's mark says what kind of thing it is;
   * a peer row's mark says which way the message went, and a thought row reads
   * as the model talking to itself.
   */
  let {
    rows,
    messages = null,
    onreadoutput = null,
  }: {
    rows: WorkRow[];
    /** The turn's messages, handed down for a dispatch row's own timeline. */
    messages?: readonly unknown[] | null;
    /** Passed straight through: a call row's ask for its own output. */
    onreadoutput?: ((callId: string) => void) | null;
  } = $props();

  /** What identifies a row: its own key, or a card's id under its own prefix. */
  function rowKey(row: WorkRow): string {
    return row.tag === 'card' ? `p-${row.card.id}` : row.key;
  }

  /** Whether a call's body is drawn without being asked for: the leaf's own rule. */
  function opens(call: ToolLeaf): boolean {
    return opensByDefault(call.name, call.body, call.decision);
  }
</script>

<div class="leaves">
  {#each rows as row (rowKey(row))}
    {#if row.tag === 'call'}
      <Call call={row.leaf} k={row.key} open={opens(row.leaf)} {messages} {onreadoutput} />
    {:else if row.tag === 'hook'}
      <Hook run={row.run} />
    {:else if row.tag === 'inbound'}
      <Inbound {row} />
    {:else if row.tag === 'thought'}
      <Thought text={row.text} />
    {:else if row.tag === 'card'}
      <PeerRow card={row.card} />
    {/if}
  {/each}
</div>
