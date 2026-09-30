<script lang="ts">
  import Chevron from '../components/Chevron.svelte';
  import Icon from '../components/Icon.svelte';
  import type { CallStatus } from './families';
  import GroupShell from './GroupShell.svelte';
  import { firstLine } from './text';
  import type { MessageKind, MessageLane, PeerCard } from './units';

  /**
   * One run of peer messages, drawn in the shared group shell: a lane per kind,
   * with every message on its own row behind a rail.
   *
   * **A message is a tool row.** It carries no tint, no border and no colour of
   * its own - the shell's own markup, lanes and marks, exactly as a run of calls
   * draws them - which is what makes traffic read as the same system rather
   * than as a second one.
   *
   * **The lane says what kind of thing it is, and the mark says who is talking.**
   * Direction is not drawn: a question this session sent and one it received
   * are both `ask`, and the mark is the counterparty's class - in this project,
   * or somewhere else.
   */
  let { lanes, status }: { lanes: MessageLane[]; status: CallStatus } = $props();

  const held = $derived(lanes.reduce((total, lane) => total + lane.cards.length, 0));

  /**
   * What names the run, which is the message it OPENED with: the rest append,
   * so the handle does not move as they arrive. The sender alone would not
   * separate two groups from one counterparty.
   */
  const key = $derived(
    `messages-${lanes[0]?.cards[0]?.peer ?? 'empty'}-${firstLine(lanes[0]?.cards[0]?.body ?? '')}`,
  );

  const drawn = $derived(
    lanes.map((lane) => ({
      key: lane.kind,
      glyph: glyphOf(lane.kind),
      label: lane.kind,
      rows: lane.cards,
    })),
  );

  /** The glyph a lane leads with: two lanes share the inbound arrow. */
  function glyphOf(kind: MessageKind): string {
    return kind === 'ask' ? 'question' : 'in';
  }

  /** A body's paragraphs, which are the blank-line breaks the prose arrives with. */
  function paragraphs(body: string): string[] {
    return body
      .split('\n\n')
      .map((one) => one.trim())
      .filter((one) => one !== '');
  }
</script>

{#snippet messageLeaf(card: PeerCard)}
  <details class="leaf">
    <summary>
      <Icon name={card.here ? 'bot' : 'away'} class="mk" />
      <span class="tn"><span class="k">{card.peer}</span> &#183; {firstLine(card.body)}</span>
      {#if card.org !== null}<span class="org">{card.org}</span>{/if}
      <Chevron />
    </summary>
    <div class="body">
      <div class="pbody">
        {#each paragraphs(card.body) as paragraph (paragraph)}
          <p>{paragraph}</p>
        {/each}
      </div>
    </div>
  </details>
{/snippet}

<GroupShell name={key} count={held} noun="message" {status} lanes={drawn} leaf={messageLeaf} />
