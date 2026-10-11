<script lang="ts">
  import { hrefForSlot } from '../routes';
  import { watchSession, type SessionRead } from '../session/live';
  import type { SessionRecord } from '../session/wire';
  import type { Connection } from '../socket';
  import type { SessionSlot } from '../wire/types';
  import {
    ageOf,
    authorName,
    loopState,
    roundTally,
    roundsNewestFirst,
    statusName,
    threadsInRound,
    turnRound,
  } from './view';

  /**
   * The review page: one seat's rounds and their threads.
   *
   * It subscribes the seat itself, as the models page subscribes its own
   * subject, and it answers nothing - the dock that answers a prompt lives in
   * a composer this page does not have.
   *
   * **Its states are drawn here rather than left to a neighbour.** The read
   * arrives as one of: nothing yet (loading), a seat the server refused (the
   * server's own sentence), the two halves read or failed apart, or both read
   * and nothing filed. A half whose outcome this build cannot name is drawn
   * as itself, because a read that answered something is not a read that
   * failed and a page that drew neither would be silent about it.
   */
  let { connection, slot }: { connection: Connection | null; slot: SessionSlot } = $props();

  let read = $state<SessionRead>({ wire: null, refused: null });

  $effect(() => {
    const open = connection;
    if (open === null) return;
    return watchSession(open, slot, false).subscribe(($next) => {
      read = $next;
    });
  });

  const record: SessionRecord | null = $derived(read.wire);
  const reviews = $derived(record?.reviews ?? null);
  const threads = $derived(reviews?.threads.outcome === 'read' ? reviews.threads.value : []);
  const rounds = $derived(
    reviews?.reviews.outcome === 'read' ? roundsNewestFirst(reviews.reviews.value) : [],
  );
  const loop = $derived(loopState(threads));
  const outdated = $derived(threads.filter((thread) => thread.status === 'outdated'));

  /** The clock the ages read against; ticking keeps a sitting page honest. */
  let now = $state(Date.now());
  $effect(() => {
    const tick = setInterval(() => (now = Date.now()), 30_000);
    return () => clearInterval(tick);
  });

  /**
   * A development door: `?seed=1` files a round through the socket, so a
   * scratch stack has something real to draw before the page can compose its
   * own. Dynamically imported behind the guard the dev fixture uses, so
   * nothing of it reaches the shipped bundle, and the flag keeps the re-read
   * that follows from seeding a second round.
   */
  let seedNote = $state<string | null>(null);
  let seeded = false;
  $effect(() => {
    if (!import.meta.env.DEV || seeded) return;
    if (new URLSearchParams(location.search).get('seed') !== '1') return;
    const open = connection;
    const branch = record?.work.branch ?? null;
    if (open === null || branch === null) return;
    seeded = true;
    void import('../dev/seed')
      .then((module) => module.seedReview(open, slot, branch))
      .then(
        (note) => (seedNote = note),
        (why: unknown) => (seedNote = `seed failed: ${String(why)}`),
      );
  });

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

<main class="wrap review">
  <header class="top">
    <a class="back" href={hrefForSlot(slot)}>&#8249; {slot.label}</a>
    <h1>Review</h1>
    {#if record !== null}
      <span class="facts">
        {record.work.branch ?? ''}
        {#if rounds.length}<span class="sep">&#183;</span> {rounds.length} rounds{/if}
      </span>
      <span class="loop" class:needs={loop.waiting > 0}>
        {#if loop.waiting > 0}
          {loop.waiting} {loop.waiting === 1 ? 'reply' : 'replies'} to read
        {:else}
          nothing waiting on you
        {/if}
        {#if loop.open > 0}<span class="sep">&#183;</span> {loop.open} open{/if}
      </span>
    {/if}
  </header>

  {#if seedNote !== null}
    <p class="half">{seedNote}</p>
  {/if}

  {#if read.refused !== null}
    <p class="pending">This forge would not answer for the seat: {read.refused}</p>
  {:else if record === null}
    <p class="pending">Reading the seat...</p>
  {:else if reviews === null}
    <p class="pending">This seat's read carried no reviews.</p>
  {:else}
    {#if reviews.threads.outcome === 'failed'}
      <p class="half bad">The threads could not be read: {reviews.threads.value}</p>
    {:else if reviews.threads.outcome === 'unknown'}
      <p class="half">
        The threads read answered <code>{String(reviews.threads.value)}</code>, which this build
        cannot name.
      </p>
    {/if}
    {#if reviews.reviews.outcome === 'failed'}
      <p class="half bad">The rounds could not be read: {reviews.reviews.value}</p>
    {:else if reviews.reviews.outcome === 'unknown'}
      <p class="half">
        The rounds read answered <code>{String(reviews.reviews.value)}</code>, which this build
        cannot name.
      </p>
    {/if}

    {#if outdated.length > 0}
      <section class="group">
        <h2>Outdated <span class="n">{outdated.length}</span></h2>
        {#each outdated as thread (thread.id)}
          <article class="thread parked">
            <div class="heads">
              <span class="state outdated">outdated</span>
              <span class="where">{thread.anchor.path} &#183; line {thread.anchor.line}</span>
            </div>
            <div class="captured">
              <div class="caphead">captured when the comment was written</div>
              <pre>{thread.anchor.context.join('\n')}</pre>
            </div>
            {#each thread.comments as turn, i (i)}
              <div class="turn" class:you={turn.author.kind === 'user'}>
                <div class="who">{authorName(turn.author)}</div>
                <p class="words">{turn.text}</p>
              </div>
            {/each}
          </article>
        {/each}
      </section>
    {/if}

    <section class="group">
      <h2>Rounds <span class="n">newest first</span></h2>
      {#if rounds.length === 0 && threads.length === 0}
        <p class="empty">Nothing has been reviewed on this branch yet.</p>
      {:else if rounds.length === 0}
        <p class="empty">
          No round has been filed. {threads.length}
          {threads.length === 1 ? 'thread is' : 'threads are'} drafted on this branch.
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
            <div class="thread">
              <div class="heads">
                <span class="state {statusName(thread.status)}">{statusName(thread.status)}</span>
                <span class="where">{thread.anchor.path} &#183; line {thread.anchor.line}</span>
              </div>
              {#each thread.comments as turn, i (i)}
                {@const sealedInto = turnRound(rounds, turn)}
                <div class="turn" class:you={turn.author.kind === 'user'}>
                  <div class="who">
                    {authorName(turn.author)}
                    {#if sealedInto !== null}<span class="tag">R{sealedInto}</span>{/if}
                  </div>
                  <p class="words">{turn.text}</p>
                </div>
              {/each}
            </div>
          {/each}
        </article>
      {/each}
    </section>
  {/if}
</main>

<style>
  /* The page's own rules, over the sheet's furniture: `wrap` and `top` are
     the app's, everything under `review` is this page's. */
  .review {
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .top {
    align-items: baseline;
  }
  .back {
    font-family: var(--mono);
    font-size: var(--fs-data);
    color: var(--dim);
    text-decoration: none;
  }
  .back:hover {
    color: var(--accent);
  }
  h1 {
    margin: 0;
    font-size: var(--fs-title);
    font-weight: 650;
    letter-spacing: -0.01em;
  }
  .facts {
    font-family: var(--mono);
    font-size: var(--fs-data);
    color: var(--dim);
  }
  .loop {
    margin-left: auto;
    font-family: var(--mono);
    font-size: var(--fs-data);
    color: var(--muted);
  }
  .loop.needs {
    color: var(--warn);
  }
  .sep {
    color: var(--line);
    margin: 0 7px;
  }

  .half {
    font-size: var(--fs-base);
    color: var(--muted);
    margin: 4px 0;
  }
  .half.bad {
    color: var(--bad);
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
  .empty {
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
  .chip {
    font-family: var(--mono);
    font-size: var(--fs-label);
    color: var(--muted);
    border: 1px solid var(--line);
    border-radius: 99px;
    padding: 0 8px;
  }
  .chip.addressed {
    color: var(--warn);
    border-color: rgba(201, 161, 59, 0.4);
  }
  .chip.resolved {
    color: var(--ok);
    border-color: rgba(130, 199, 107, 0.35);
  }
  .chip.outdated {
    color: var(--dim);
  }
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
  .state {
    font-family: var(--mono);
    font-size: var(--fs-label);
    color: var(--muted);
    border: 1px solid var(--line);
    border-radius: 99px;
    padding: 0 8px;
  }
  .state.open {
    color: var(--text);
    border-color: var(--ctl);
  }
  .state.addressed {
    color: var(--warn);
    border-color: rgba(201, 161, 59, 0.4);
  }
  .state.resolved {
    color: var(--ok);
    border-color: rgba(130, 199, 107, 0.35);
  }
  .state.outdated {
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

  .turn {
    border-left: 1px solid var(--line);
    padding: 2px 0 2px 10px;
    margin: 7px 0;
  }
  .turn.you {
    border-left-color: var(--accent);
  }
  .who {
    display: flex;
    gap: 8px;
    font-family: var(--mono);
    font-size: var(--fs-label);
    color: var(--dim);
  }
  .turn.you .who {
    color: var(--muted);
  }
  .tag {
    color: var(--dim);
  }
  .words {
    margin: 1px 0 0;
    font-size: var(--fs-base);
    color: var(--muted);
    overflow-wrap: anywhere;
    max-width: 72ch;
  }
  .turn.you .words {
    color: var(--text);
  }
</style>
