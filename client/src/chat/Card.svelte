<script lang="ts">
  import Chevron from '../components/Chevron.svelte';
  import Icon from '../components/Icon.svelte';
  import { firstLine } from './text';
  import type { AnsweredQuestion, PeerCard } from './units';

  /**
   * A card the conversation carries that is not prose or a call: a question
   * the assistant asked, a peer message, or a run of them.
   *
   * **The marks are the sprite's, not a character cell's.** The question card
   * led its answers with a right arrow because a terminal had nothing else to
   * draw with, and it reads as punctuation beside the icons on every other
   * row.
   */
  let {
    kind,
    asked = [],
    cards = [],
    card = null,
  }: {
    kind: 'question' | 'peer' | 'peers';
    asked?: AnsweredQuestion[];
    cards?: PeerCard[];
    card?: PeerCard | null;
  } = $props();

  /** The messages a group draws, which for a lone card is the one it is. */
  const held = $derived(card === null ? cards : [card]);

  /**
   * What names a card, which is what a view keys its open state on.
   *
   * The FIRST message's own identity for a run, never the count: a group grows
   * as messages arrive, so a key carrying its length renames the row every
   * time one lands - which is the one thing a key must not do.
   */
  const key = $derived(
    card !== null
      ? `peer-${card.peer}-${card.body}`
      : `peers-${cards[0]?.peer ?? ''}-${cards[0]?.body ?? ''}`,
  );
</script>

{#if kind === 'question'}
  {#each asked as pair, at (at)}
    <div class="card">
      <div class="q">
        <Icon name="question" class="qm" />
        {pair.question}
      </div>
      {#each pair.picked_labels as picked (picked)}
        <div class="a">
          <Icon name="check" class="am" />
          <span class="picked">{picked}</span>
        </div>
      {/each}
      {#if pair.typed_note !== null}
        <div class="a">
          <span class="am">you typed:</span>
          <span class="typed">{pair.typed_note}</span>
        </div>
      {/if}
    </div>
  {/each}
{:else if kind === 'peer' && card !== null}
  <div class="peer">
    <Icon name={card.inbound ? 'in' : 'out'} class="dir" />
    <span class="from">{card.peer}</span>
    <span class="txt">{firstLine(card.body)}</span>
  </div>
{:else if kind === 'peers'}
  <details class="msg" open data-k={key}>
    <summary>
      <Icon name="check" class="st" />
      <span class="c">{held.length} messages</span>
      <Chevron />
    </summary>
    <div class="msgbody">
      {#each held as message, at (at)}
        <div class="mmsg">
          <Icon name={message.inbound ? 'in' : 'out'} class="dir" />
          <span class="kb">{message.kind}</span>
          <span class="who">{message.peer}</span>
          <span class="txt">{firstLine(message.body)}</span>
        </div>
      {/each}
    </div>
  </details>
{/if}
