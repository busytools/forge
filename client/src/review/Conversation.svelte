<script lang="ts">
  /**
   * The conversation: one scope's rounds, newest first, each holding its own
   * threads, with the parked ones above as their own group - **one draw for
   * both pages that hold a review** (the review page's branch, the files
   * page's seat tree; Ved, 2026-10-17: "review it should ideally work just
   * like how the git diff").
   *
   * The page owns the state - the reply box, the busy flag, what a resolve
   * files - and this owns the arrangement: which threads sit in which round,
   * the tally chips, the ages, and the captured lines a parked thread keeps.
   */
  import type { Snippet } from 'svelte';

  import type { ReviewSetRecord, ReviewThreadRecord } from '../session/wire';
  import type { ReplyBox, Voice } from './boxes';
  import ThreadTurn from './ThreadTurn.svelte';
  import WordsBox from './WordsBox.svelte';
  import {
    ageOf,
    outdatedThreads,
    parkedReason,
    roundTally,
    statusName,
    threadsInRound,
  } from './view';

  let {
    threads,
    rounds,
    drafts,
    busy,
    now,
    voice,
    reply,
    noun = 'branch',
    known = true,
    wide = false,
    onreply,
    onresolve,
    onreopen,
    onpress,
    onhold,
    onfield,
    footer,
  }: {
    threads: ReviewThreadRecord[];
    rounds: ReviewSetRecord[];
    /** The threads carrying a turn nobody filed - the rows drawing `drafted`. */
    drafts: ReadonlySet<string>;
    busy: boolean;
    now: number;
    voice: Voice;
    reply: ReplyBox;
    /** The word the empty lines name the scope with: 'branch', 'tree'. */
    noun?: string;
    /** Whether both reads landed; the empty lines wait for them. */
    known?: boolean;
    /** Take the whole row rather than the reading measure - for a page whose
     *  conversation is not a column beside its own panes. */
    wide?: boolean;
    /** File one reply. False is a refusal, and the words are the reader's still. */
    onreply: (thread: ReviewThreadRecord, text: string) => Promise<boolean>;
    onresolve: (thread: ReviewThreadRecord) => void;
    onreopen: (thread: ReviewThreadRecord) => void;
    onpress: () => void;
    onhold: () => void;
    onfield: (el: HTMLElement | null) => void;
    footer?: Snippet;
  } = $props();

  const outdated = $derived(outdatedThreads(threads));

  function tallyChips(tally: ReturnType<typeof roundTally>): [string, number][] {
    const rows: [string, number][] = [
      ['open', tally.open],
      ['addressed', tally.addressed],
      ['resolved', tally.resolved],
      ['outdated', tally.outdated],
    ];
    return rows.filter(([, count]) => count > 0);
  }
</script>

<div class="rv-conv" class:wide>
  {#if outdated.length > 0}
    <section class="group">
      <h2>Outdated <span class="n">{outdated.length}</span></h2>
      {#each outdated as thread (thread.id)}
        {@const inRound = rounds.some((round) =>
          threadsInRound(threads, round.id).some((held) => held.id === thread.id),
        )}
        <article class="thread parked">
          <div class="heads">
            <span class="rv-state outdated">outdated</span>
            <span class="where">{thread.anchor.path} &#183; line {thread.anchor.line}</span>
          </div>
          {#if parkedReason(thread) !== ''}
            <p class="rv-why">{parkedReason(thread)}</p>
          {/if}
          <div class="captured">
            <div class="caphead">captured when the comment was written</div>
            <pre>{thread.anchor.context.join('\n')}</pre>
          </div>
          {#each thread.comments as turn, i (i)}
            <ThreadTurn {turn} {rounds} {now} index={i} />
          {/each}
          <div class="acts">
            {#if inRound}
              <!-- Only a thread a round holds draws a reply box, so the door
                   is offered only where the box is. -->
              <button
                type="button"
                class="rv-a"
                disabled={busy}
                onclick={() => {
                  reply.thread = reply.thread === thread.id ? null : thread.id;
                  reply.draft = '';
                }}>respond</button
              >
            {/if}
            <button type="button" class="rv-a" disabled={busy} onclick={() => onresolve(thread)}
              >resolve</button
            >
            {#if drafts.has(thread.id)}<span class="rv-cc">drafted</span>{/if}
          </div>
        </article>
      {/each}
    </section>
  {/if}

  <section class="group">
    <h2>Rounds <span class="n">newest first</span></h2>
    {#if known && rounds.length === 0 && threads.length === 0}
      <p class="rv-empty">Nothing has been reviewed on this {noun} yet.</p>
    {:else if known && rounds.length === 0}
      <p class="rv-empty">
        No round has been filed. {threads.length}
        {threads.length === 1 ? 'thread is' : 'threads are'} drafted on this {noun}.
      </p>
    {/if}
    {#each rounds as round (round.id)}
      {@const tally = roundTally(threads, round)}
      <article class="round">
        <div class="heads">
          <span class="rn">R{round.number}</span>
          <span class="age">{ageOf(round.created_at, now)}</span>
          <span class="tally">
            {#each tallyChips(tally) as [state, count] (state)}
              <span class="chip {state}">{count} {state}</span>
            {/each}
            {#if tally.named > 0}
              <span class="chip">{tally.named} unnamed</span>
            {/if}
          </span>
        </div>
        {#if round.summary !== null}<p class="summary">{round.summary}</p>{/if}
        {#each threadsInRound(threads, round.id) as thread (thread.id)}
          {@const name = statusName(thread.status)}
          <div class="thread">
            <div class="heads">
              <span class="rv-state {name}">{name}</span>
              <span class="where">{thread.anchor.path} &#183; line {thread.anchor.line}</span>
            </div>
            {#each thread.comments as turn, i (i)}
              <ThreadTurn {turn} {rounds} {now} index={i} />
            {/each}
            <div class="acts">
              <button
                type="button"
                class="rv-a"
                disabled={busy}
                onclick={() => {
                  reply.thread = reply.thread === thread.id ? null : thread.id;
                  reply.draft = '';
                }}>respond</button
              >
              {#if name === 'resolved'}
                <button type="button" class="rv-a" disabled={busy} onclick={() => onreopen(thread)}
                  >reopen</button
                >
              {:else}
                <button type="button" class="rv-a" disabled={busy} onclick={() => onresolve(thread)}
                  >resolve</button
                >
              {/if}
              {#if drafts.has(thread.id)}<span class="rv-cc">drafted</span>{/if}
            </div>
            {#if reply.thread === thread.id}
              <!-- The same box the canvas mounts under its threads: one
                   component, so a reply reads and dictates the same wherever
                   it is drawn. -->
              <WordsBox
                class="rv-reply"
                rows={2}
                placeholder="Respond to this thread..."
                label="your response"
                sendLabel="send"
                box={reply}
                {voice}
                onsend={(text: string) => {
                  void onreply(thread, text).then((filed) => {
                    // A refusal keeps the words: the reply is the reader's
                    // until the server took it.
                    if (!filed) return;
                    reply.thread = null;
                    reply.draft = '';
                  });
                }}
                oncancel={() => {
                  reply.thread = null;
                  reply.draft = '';
                }}
                {onpress}
                {onhold}
                field={onfield}
              />
            {/if}
          </div>
        {/each}
      </article>
    {/each}
  </section>

  {#if footer}{@render footer()}{/if}
</div>

<style>
  /* The rounds read as a column, centred: a measure cap that is not centred
     is a hole. */
  .rv-conv {
    max-width: 820px;
    margin: 0 auto;
    width: 100%;
    overflow-y: auto;
    min-height: 0;
    scrollbar-gutter: stable;
  }
  /* The page with nothing to keep the space for: the rounds run the full row
     (Ved, 2026-10-17: "use as much space as you can"). */
  .rv-conv.wide {
    max-width: none;
  }
  .group {
    margin: 14px 0 8px;
  }
  .group h2 {
    display: flex;
    gap: 8px;
    align-items: baseline;
    margin: 0 0 6px;
    font-size: var(--fs-label);
    font-weight: 700;
    letter-spacing: 0.12em;
    text-transform: uppercase;
    color: var(--dim);
  }
  .n {
    font-family: var(--mono);
    letter-spacing: 0;
  }
  .rv-empty {
    font-size: var(--fs-base);
    color: var(--dim);
    margin: 2px 0 8px;
  }

  .round {
    border: 1px solid var(--line);
    border-radius: var(--r);
    background: var(--s1);
    padding: 9px 11px 10px;
    margin-bottom: 10px;
  }
  .heads {
    display: flex;
    align-items: baseline;
    gap: 9px;
  }
  .rn {
    font-family: var(--mono);
    font-size: var(--fs-base);
    font-weight: 600;
    color: var(--accent);
  }
  .age {
    font-family: var(--mono);
    font-size: var(--fs-label);
    color: var(--dim);
  }
  .tally {
    margin-left: auto;
    display: flex;
    gap: 6px;
  }
  /* The `.chip` set lives in the sheet (web.css): the strip's review panel
     wears the same chips, and one definition is the rule. */
  .summary {
    font-size: var(--fs-base);
    color: var(--muted);
    margin: 6px 0 2px;
  }

  .thread {
    border-top: 1px solid rgba(34, 42, 60, 0.55);
    margin-top: 8px;
    padding-top: 8px;
  }
  .thread:first-of-type {
    border-top: 0;
    margin-top: 4px;
    padding-top: 4px;
  }
  .thread.parked {
    border: 1px solid rgba(120, 130, 148, 0.35);
    border-radius: var(--r);
    padding: 9px 11px 10px;
    margin: 0 0 10px;
    background: var(--s1);
  }
  .rv-state {
    font-family: var(--mono);
    font-size: var(--fs-label);
    color: var(--muted);
    border: 1px solid var(--line);
    border-radius: 99px;
    padding: 0 8px;
  }
  .rv-state.open {
    color: var(--text);
    border-color: var(--ctl);
  }
  .rv-state.addressed {
    color: var(--warn);
    border-color: rgba(201, 161, 59, 0.4);
  }
  .rv-state.resolved {
    color: var(--ok);
    border-color: rgba(130, 199, 107, 0.35);
  }
  .rv-state.outdated {
    color: var(--dim);
  }
  .where {
    font-family: var(--mono);
    font-size: var(--fs-label);
    color: var(--dim);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .captured {
    margin: 7px 0;
    border: 1px solid var(--line);
    border-radius: 8px;
    background: var(--s2);
    overflow: hidden;
  }
  .caphead {
    font-family: var(--mono);
    font-size: var(--fs-label);
    color: var(--dim);
    padding: 4px 10px;
    border-bottom: 1px solid var(--line);
  }
  .captured pre {
    margin: 0;
    padding: 6px 10px;
    font-family: var(--mono);
    font-size: var(--fs-label);
    color: var(--muted);
    white-space: pre-wrap;
    overflow-wrap: anywhere;
  }

  /* The one column scrolls far past a refusal written above it, so at this
     width the conversation's own box rides the page instead. */
  @media (max-width: 980px) {
    .rv-conv {
      overflow: visible;
    }
  }
</style>
