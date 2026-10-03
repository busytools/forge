<script lang="ts">
  import { flip } from 'svelte/animate';
  import Call from './Call.svelte';
  import Chevron from '../components/Chevron.svelte';
  import Icon from '../components/Icon.svelte';
  import { iconOf } from './families';
  import Hook from './Hook.svelte';
  import Inbound from './Inbound.svelte';
  import { opensByDefault, type ToolLeaf } from './leaves';
  import Prose from './Prose.svelte';
  import { renderInlineProse } from './prose';
  import { firstLine, joinedLine, paragraphs } from './text';
  import type {
    CallLeaf,
    HookLeaf,
    InboundKind,
    InboundLeaf,
    Lane,
    PeerCard,
    PeerRow,
  } from './units';

  /** A thought as its lane draws it: the words, and the row's line rendered. */
  type DrawnThought = { key: string; text: string; lead: string };

  /** A peer row's title, in the three parts the row draws: lead, name, tail. */
  type DrawnTitle = { lead: string; name: string; tail: string };

  /**
   * A stretch of the turn's work: a lane per tool family, one peer lane, the
   * thinking, one for a hook's runs and one per external delivery kind, with
   * every call, message or thought on its own row behind a rail.
   *
   * **No disclosure around the group.** The count and the roll-up are not
   * drawn: a stretch of work is not a thing to hide, the lane rows say what
   * ran, and every row still opens on its own - so a reader who wants less
   * opens the rows rather than the group.
   *
   * **A message is a tool row, and so is a thought.** They carry no tint, no
   * border and no colour of their own - the same lanes and marks a run of calls
   * draws - which is what makes traffic and reasoning read as the same system
   * rather than as a second one. The lane says what kind of thing it is; a
   * peer row's mark says which way the message went, and a thought lane is the
   * model talking to itself.
   *
   * **A lane that takes a row moves to the bottom of the group**, and the move
   * is animated: it is what tells the reader which lane just changed. The fold
   * fixes the order, this only draws where each lane ends up.
   */
  let { lanes }: { lanes: Lane[] } = $props();

  /**
   * Whether the move is drawn.
   *
   * The sheet's own reduced-motion rule covers CSS animation; a flip is a
   * transform written from script, so it reads the preference itself.
   */
  const still =
    typeof matchMedia === 'function' && matchMedia('(prefers-reduced-motion: reduce)').matches;

  // A label is not an identity: the fold draws a family as `(label, row kind)`,
  // and a built-in beside an MCP server named after it - a `Read` and an
  // `mcp__read__query` - is two families with one word. Keying a lane by the
  // word alone is a duplicate key, and a duplicate key stops the whole turn
  // drawing at mount.
  const drawn = $derived(
    lanes.map((lane) =>
      lane.tag === 'family'
        ? {
            key: `f:${lane.label}-${lane.row.kind}`,
            glyph: iconOf(lane.row),
            label: lane.label,
            calls: lane.calls,
            cards: [] as PeerCard[],
            thoughts: [] as DrawnThought[],
            hooks: [] as HookLeaf[],
            inbounds: [] as InboundLeaf[],
          }
        : lane.tag === 'message'
          ? {
              key: 'm:peer',
              glyph: 'bot',
              label: 'peer',
              calls: [] as CallLeaf[],
              cards: lane.cards,
              thoughts: [] as DrawnThought[],
              hooks: [] as HookLeaf[],
              inbounds: [] as InboundLeaf[],
            }
          : lane.tag === 'thought'
            ? {
                key: 't:think',
                glyph: 'brain',
                label: 'thinking',
                calls: [] as CallLeaf[],
                cards: [] as PeerCard[],
                // The row's line is the text joined and unblocked, its inline
                // marks rendered: a preview cannot draw a heading, and it can
                // draw emphasis and code.
                thoughts: lane.thoughts.map((thought) => ({
                  ...thought,
                  lead: renderInlineProse(joinedLine(thought.text)),
                })),
                hooks: [] as HookLeaf[],
                inbounds: [] as InboundLeaf[],
              }
            : lane.tag === 'hook'
              ? {
                  key: 'h:hook',
                  glyph: 'hook',
                  label: 'hook',
                  calls: [] as CallLeaf[],
                  cards: [] as PeerCard[],
                  thoughts: [] as DrawnThought[],
                  hooks: lane.runs,
                  inbounds: [] as InboundLeaf[],
                }
              : {
                  key: `d:${lane.kind}`,
                  glyph: inboundGlyph(lane.kind),
                  label: lane.kind,
                  calls: [] as CallLeaf[],
                  cards: [] as PeerCard[],
                  thoughts: [] as DrawnThought[],
                  hooks: [] as HookLeaf[],
                  inbounds: lane.rows,
                },
    ),
  );

  /** The glyph an inbound lane leads with. */
  function inboundGlyph(kind: InboundKind): string {
    return kind === 'cron' ? 'schedules' : kind;
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

  /** Whether a call's body is drawn without being asked for: a mutation's diff, while it is small enough to draw. */
  function opens(call: ToolLeaf): boolean {
    return opensByDefault(call.name, call.body);
  }
</script>

<div class="leaves">
  {#each drawn as lane (lane.key)}
    <div class="lane" animate:flip={{ duration: still ? 0 : 220 }}>
      <div class="knd">
        <Icon name={lane.glyph} class="gl" />
        <span class="nm">{lane.label}</span>
      </div>
      {#each lane.calls as call (call.key)}
        <Call call={call.leaf} open={opens(call.leaf)} />
      {/each}
      {#each lane.cards as card (card.id)}
        {@const title = titleOf(card)}
        <details class="leaf">
          <summary>
            <Icon name={markOf(card.row)} class="mk" />
            <span class="tn" class:warn={card.row === 'failed'}>
              {#if title.lead !== ''}<span class="dir">{title.lead}</span>{/if}<span class="k"
                >{title.name}</span
              >
              &#183; {title.tail}
            </span>
            {#if card.org !== null}<span class="org">{card.org}</span>{/if}
            <Chevron />
          </summary>
          <div class="body">
            <div class="pbody">
              {#each paragraphs(card.body) as paragraph, at (at)}
                <p>{paragraph}</p>
              {/each}
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
        </details>
      {/each}
      {#each lane.hooks as held (held.key)}
        <Hook run={held.run} />
      {/each}
      {#each lane.inbounds as inbound (inbound.key)}
        <Inbound row={inbound} />
      {/each}
      {#each lane.thoughts as thought (thought.key)}
        <!-- The body is markdown: the model writes its reasoning in headings,
             lists and code, and the terminal has no row for it at all. -->
        <details class="leaf">
          <summary>
            <Icon name="lightbulb" class="st bulb" />
            <!--
              The row's rendered line, which the module produced from escaped
              input: same renderer as the body, raw HTML off.
            -->
            <!-- eslint-disable-next-line svelte/no-at-html-tags -->
            <span class="tn">{@html thought.lead}</span>
            <Chevron />
          </summary>
          <div class="body">
            <Prose text={thought.text} />
          </div>
        </details>
      {/each}
    </div>
  {/each}
</div>
