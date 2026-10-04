<script lang="ts">
  import Call from './Call.svelte';
  import Chevron from '../components/Chevron.svelte';
  import Icon from '../components/Icon.svelte';
  import Hook from './Hook.svelte';
  import Inbound from './Inbound.svelte';
  import { opensByDefault, type ToolLeaf } from './leaves';
  import Prose from './Prose.svelte';
  import { renderInlineProse } from './prose';
  import { firstLine, joinedLine } from './text';
  import type { PeerCard, PeerRow, WorkRow } from './units';

  /** A peer row's title, in the three parts the row draws: lead, name, tail. */
  type DrawnTitle = { lead: string; name: string; tail: string };

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
  let { rows }: { rows: WorkRow[] } = $props();

  /** What identifies a row: its own key, or a card's id under its own prefix. */
  function rowKey(row: WorkRow): string {
    return row.tag === 'card' ? `p-${row.card.id}` : row.key;
  }

  /**
   * The mark a peer row carries.
   *
   * **The words carry the direction; the mark reinforces them.** A message is
   * a plane flying out or an inbox filling up, a failure keeps the outgoing
   * mark it belongs to, and the two verb cards mark themselves.
   */
  function markOf(row: PeerRow): string {
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

  /** Whether a call's body is drawn without being asked for: the leaf's own rule. */
  function opens(call: ToolLeaf): boolean {
    return opensByDefault(call.name, call.body, call.decision);
  }
</script>

<div class="leaves">
  {#each rows as row (rowKey(row))}
    {#if row.tag === 'call'}
      <Call call={row.leaf} k={row.key} open={opens(row.leaf)} />
    {:else if row.tag === 'hook'}
      <Hook run={row.run} />
    {:else if row.tag === 'inbound'}
      <Inbound {row} />
    {:else if row.tag === 'thought'}
      {@const lead = renderInlineProse(joinedLine(row.text))}
      <!-- The body is markdown: the model writes its reasoning in headings,
           lists and code, and the terminal has no row for it at all. -->
      <details class="leaf">
        <summary>
          <Icon name="message-circle-more" class="gl" />
          <!--
            The row's rendered line, which the module produced from escaped
            input: same renderer as the body, raw HTML off.
          -->
          <!-- eslint-disable-next-line svelte/no-at-html-tags -->
          <span class="tn">{@html lead}</span>
          <Chevron />
        </summary>
        <div class="body">
          <Prose text={row.text} />
        </div>
      </details>
    {:else if row.tag === 'card'}
      {@const title = titleOf(row.card)}
      <details class="leaf">
        <summary>
          <Icon name={markOf(row.card.row)} class="mk" />
          <span class="tn" class:warn={row.card.row === 'failed'}>
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
          {#if row.card.org !== null}<span class="org">{row.card.org}</span>{/if}
          <Chevron />
        </summary>
        <div class="body">
          <div class="pbody">
            <!-- A peer message is prose from another session, so it reads the
                 way every other message does: marks rendered, not shown. -->
            <Prose text={row.card.body} />
            {#if row.card.ack !== null}
              <div class="kv"><span class="k">sent</span><span class="v">{row.card.ack}</span></div>
            {/if}
            {#if row.card.seat !== null}
              <div class="kv">
                <span class="k">org</span><span class="v">{row.card.seat.org}</span>
              </div>
              <div class="kv">
                <span class="k">project</span><span class="v">{row.card.seat.project}</span>
              </div>
              <div class="kv">
                <span class="k">label</span><span class="v">{row.card.seat.label}</span>
              </div>
              <div class="kv">
                <span class="k">path</span><span class="v">{row.card.seat.path}</span>
              </div>
              <div class="kv">
                <span class="k">status</span><span class="v">{row.card.seat.status}</span>
              </div>
            {/if}
            <!-- Keyed by the slot, not the label: every project's own agent
                 is labelled `lead`, so an unfiltered list would key two rows
                 to one name and stop the whole turn drawing at mount. -->
            {#each row.card.seats as seat (`${seat.org}/${seat.project}/${seat.label}`)}
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
      </details>
    {/if}
  {/each}
</div>
