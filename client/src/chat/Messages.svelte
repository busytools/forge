<script lang="ts">
  import Chevron from '../components/Chevron.svelte';
  import Icon from '../components/Icon.svelte';
  import { firstLine } from './text';
  import type { MessageKind, MessageLane } from './units';

  /**
   * One run of peer messages: the count, then a lane per kind with every
   * message behind a rail.
   *
   * **A message is a tool row.** It carries no tint, no border and no colour of
   * its own - it is the same `.kind` / `.knd` / `details.leaf` markup a run of
   * calls is, which is what makes traffic read as the same system rather than
   * as a second one.
   *
   * **The lane says what kind of thing it is, and the mark says who is talking.**
   * Direction is not drawn: a question this session sent and one it received
   * are both `ask`, and the mark is the counterparty's class - in this project,
   * or somewhere else.
   */
  let { lanes }: { lanes: MessageLane[] } = $props();

  const held = $derived(lanes.reduce((total, lane) => total + lane.cards.length, 0));

  /** What names the run, which is the message it OPENED with: the rest append. */
  const key = $derived(`messages-${lanes[0]?.cards[0]?.peer ?? 'empty'}`);

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

<details class="kind" open data-k={key}>
  <summary>
    <Icon name="check" class="st" />
    <span class="nm">{held} {held === 1 ? 'message' : 'messages'}</span>
    <Chevron />
  </summary>
  <div class="leaves">
    {#each lanes as lane (lane.kind)}
      <div class="knd">
        <Icon name={glyphOf(lane.kind)} class="gl" />
        <span class="nm">{lane.kind}</span>
      </div>
      {#each lane.cards as card, at (at)}
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
      {/each}
    {/each}
  </div>
</details>
