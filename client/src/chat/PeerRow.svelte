<script lang="ts">
  import { untrack } from 'svelte';

  import Chevron from '../components/Chevron.svelte';
  import Icon from '../components/Icon.svelte';
  import Prose from './Prose.svelte';
  import { renderInlineProse } from './prose';
  import { firstLine, joinedLine } from './text';
  import type { PeerCard, PeerRow as PeerRowKind } from './units';

  /** A peer row's title, in the three parts the row draws: lead, name, tail. */
  type DrawnTitle = { lead: string; name: string; tail: string };

  /**
   * One peer card, as its own row: the direction, the counterparty and the
   * message's first line on the summary, everything else behind the open.
   *
   * **A closed row carries its summary and nothing else** - the giant seat's
   * own cost is the bodies of rows nobody opened - so the body is drawn onto
   * the open.
   */
  let { card, open = false }: { card: PeerCard; open?: boolean } = $props();

  /** Whether the row is open, held here rather than drawn from the prop. */
  let opened = $state(untrack(() => open));

  /**
   * The mark a peer row carries.
   *
   * **The words carry the direction; the mark reinforces them.** A message is
   * a plane flying out or an inbox filling up, a failure keeps the outgoing
   * mark it belongs to, and the two verb cards mark themselves.
   */
  function markOf(row: PeerRowKind): string {
    switch (row) {
      case 'sent':
      case 'failed':
        return 'plane';
      case 'arrived':
        return 'inbox';
      case 'whoami':
        return 'badge';
      case 'list':
        return 'users';
    }
  }

  /**
   * A peer row's title: the direction words, the failure's own sentence, or
   * the verb's card.
   */
  function titleOf(card: PeerCard): DrawnTitle {
    switch (card.row) {
      case 'sent':
        return { lead: 'sent to', name: card.peer, tail: firstLine(card.body) };
      case 'arrived':
        return { lead: 'from', name: card.peer, tail: firstLine(card.body) };
      case 'failed':
        return { lead: '', name: card.peer, tail: `failed to deliver: ${card.body}` };
      case 'whoami':
        return { lead: '', name: 'whoami', tail: 'where this session sits' };
      case 'list':
        return { lead: '', name: 'list', tail: 'who you can send to' };
    }
  }

  const title = $derived(titleOf(card));
</script>

<details class="leaf" bind:open={opened}>
  <summary>
    <Icon name={markOf(card.row)} class="mk" />
    <span class="tn" class:warn={card.row === 'failed'}>
      {#if title.lead !== ''}<span class="dir">{title.lead}</span>{/if}<span class="k"
        >{title.name}</span
      >
      <!-- The row's line is the preview with its inline marks rendered,
           as the thought row's own line is: a preview shows emphasis
           and code, and cannot draw a heading. -->
      &#183;
      <!-- eslint-disable-next-line svelte/no-at-html-tags -->
      {@html renderInlineProse(joinedLine(title.tail))}
    </span>
    {#if card.org !== null}<span class="org">{card.org}</span>{/if}
    <Chevron />
  </summary>
  {#if opened}
    <div class="body">
      <div class="pbody">
        <!-- A peer message is prose from another session, so it reads the
             way every other message does: marks rendered, not shown. -->
        <Prose text={card.body} />
        {#if card.ack !== null}
          <div class="kv"><span class="k">sent</span><span class="v">{card.ack}</span></div>
        {/if}
        {#if card.seat !== null}
          <div class="kv">
            <span class="k">org</span><span class="v">{card.seat.org}</span>
          </div>
          <div class="kv">
            <span class="k">project</span><span class="v">{card.seat.project}</span>
          </div>
          <div class="kv">
            <span class="k">label</span><span class="v">{card.seat.label}</span>
          </div>
          <div class="kv">
            <span class="k">path</span><span class="v">{card.seat.path}</span>
          </div>
          <div class="kv">
            <span class="k">status</span><span class="v">{card.seat.status}</span>
          </div>
        {/if}
        <!-- Keyed by the slot, not the label: every project's own agent
             is labelled `lead`, so an unfiltered list would key two rows
             to one name and stop the whole turn drawing at mount. -->
        {#each card.seats as seat (`${seat.org}/${seat.project}/${seat.label}`)}
          <div class="kv">
            <span class="k">{seat.label}</span>
            <!-- A project's own agent has no activity to report, so its
                 row is two facts and no trailing separator. -->
            <span class="v"
              >{seat.project} &#183; {seat.what}{seat.liveness === ''
                ? ''
                : ` \u{b7} ${seat.liveness}`}</span
            >
          </div>
        {/each}
      </div>
    </div>
  {/if}
</details>
