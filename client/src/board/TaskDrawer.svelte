<script lang="ts">
  import Icon from '../components/Icon.svelte';
  import { hrefForSlot } from '../routes';
  import type { ServiceReport } from '../wire/fleet';
  import type { BoardRow, ByWire, TaskStatus } from '../wire/home';
  import { followable } from '../home/view';
  import { artifactOf, fmtSecs, waitOnUser, type WaitingView } from './view';

  /**
   * One row's own page, over the board: what the row IS (its fields and
   * links), what it is waiting on - answerable here, with the same commands
   * the board's waiting strip sends - and where it has been (its own
   * transitions, which the core already reads for the worked time).
   *
   * The board stays behind the drawer, so a reader never loses the lane they
   * were reading. Everything it draws comes off the row it is handed; it
   * holds no state of its own but the answer boxes.
   */
  let {
    row,
    waiting = null,
    line = null,
    now,
    onact,
    onclose,
    tasks = [],
    onjump = null,
  }: {
    row: BoardRow;
    /**
     * The row's waiting view, where it waits. The approve gate reads THIS,
     * not the rollup: `root` and `open` are the core's own close rule (a
     * root closes only when every child is terminal), so the control offered
     * here is exactly the control the strip offers.
     */
    waiting?: WaitingView | null;
    /** The board's own report line: a command sent from here that did not land. */
    line?: ServiceReport | null;
    now: number;
    onact: (command: Record<string, Record<string, unknown>>) => boolean;
    onclose: () => void;
    /** A parent's own tasks, each labelled with its lane's word; empty for a row without children. */
    tasks?: { id: string; label: string }[];
    /** The board's jump, so picking one of those tasks lands on its card. */
    onjump?: ((id: string) => void) | null;
  } = $props();

  const task = $derived(row.task);
  const seatHref = $derived(task.owner === null ? null : hrefForSlot(task.owner));

  /**
   * The artifact a look is asking the reader to see: the first followable
   * PR or path link on a look wait. The reader should not have to hunt the
   * row's field list to find what to look at (#2423 look).
   */
  const lookArtifact = $derived(
    task.waiting_on !== null && task.waiting_on.verification && waitOnUser(task)
      ? artifactOf(task)
      : null,
  );

  /**
   * The ask block's references. The artifact the door presents is dropped
   * from this line: one reference, one showing. The fields above still
   * carry the row's whole list - that is the record, not the ask's doors.
   */
  const refs = $derived(
    task.links
      .map((link) => ({ label: link.label ?? link.target, href: followable(link.target) }))
      .filter((link) => lookArtifact === null || link.href !== lookArtifact.href),
  );

  /** A row can take a reservation while it is still live, not once it ended. */
  const live = $derived(
    task.status !== 'completed' && task.status !== 'failed' && task.status !== 'canceled',
  );

  /** The row's prose is the ask itself when the wait states none. */
  const detailIsAsk = $derived(
    task.waiting_on !== null && task.waiting_on.detail === null && task.detail !== null,
  );

  /** How many moves the row's own carry withholds, from its whole count. */
  const withheld = $derived(Math.max(0, row.history_total - row.history.length));

  /** Words for the state a transition moved TO. */
  function movedWords(to: TaskStatus): string {
    switch (to) {
      case 'in_progress':
        return 'in progress';
      case 'completed':
        return 'completed';
      case 'failed':
        return 'failed';
      case 'canceled':
        return 'canceled';
      case 'waiting':
        return 'waiting';
      case 'pending':
        return 'backlog';
    }
  }

  /** Who moved it: a seat's label, the reader, forge's own machinery, or a
   * mover a newer server invented (narrowed to `unknown` at the boundary). */
  function byWords(by: ByWire): string {
    if (by === 'user') return 'you';
    if (by === 'system') return 'forge';
    if (typeof by === 'object') return by.seat.label;
    return 'an unknown mover';
  }

  /** The clock of a transition's instant, HH:MM in the reader's own zone. */
  function atWords(secs: number): string {
    const at = new Date(secs * 1_000);
    return `${at.getHours()}:${String(at.getMinutes()).padStart(2, '0')}`;
  }

  const created = $derived(atWords(task.created_at.secs_since_epoch));
  const updated = $derived(atWords(task.updated_at.secs_since_epoch));
  const ago = $derived(
    fmtSecs(Math.max(0, Math.floor(now / 1_000) - task.updated_at.secs_since_epoch)),
  );
  /** The estimate's half of the worked line: the leading space lives in the
      string, where the compiler cannot trim it off the number. */
  const estimateWords = $derived(task.estimate === null ? '' : ` of ${task.estimate.words}`);

  let answer = $state('');
  let sendBack = $state('');

  /** The drawer takes the keyboard as it mounts and on Escape gives it back. */
  let root = $state<HTMLElement | null>(null);
  $effect(() => {
    root?.focus();
  });

  function keydown(event: KeyboardEvent): void {
    if (event.key === 'Escape') {
      event.stopPropagation();
      onclose();
    }
  }
</script>

<svelte:window onkeydown={keydown} />

<div class="dr-scrim" onclick={onclose} role="presentation"></div>
<div
  class="dr"
  role="dialog"
  aria-modal="true"
  aria-label="the task"
  tabindex="-1"
  bind:this={root}
>
  <div class="dr-head" class:tree={row.parent_subject !== null}>
    {#if task.status === 'waiting'}
      <span class="dr-state warn">{waitOnUser(task) ? 'waiting on you' : 'waiting'}</span>
    {:else if task.status === 'completed'}
      <span class="dr-state ok"><Icon name="check" /> done</span>
    {:else if task.status === 'failed'}
      <span class="dr-state bad">failed</span>
    {:else if task.status === 'in_progress'}
      <span class="dr-state run">in progress</span>
    {:else}
      <span class="dr-state">{movedWords(task.status)}</span>
    {/if}
    <div class="dr-title">
      <div class="t">{task.subject}</div>
      {#if task.active_form !== null}<div class="sub">{task.active_form}</div>{/if}
    </div>
    <button class="dr-x" type="button" aria-label="close" onclick={onclose}>&#10005;</button>
  </div>

  <div class="dr-body">
    {#if line !== null}
      <!-- The board's line, drawn here too: a refused approve or answer sent
           from the drawer otherwise says nothing a reader can see. -->
      <p class="dr-said {line.severity}" role="status">{line.message}</p>
    {/if}
    <dl class="dr-fields">
      <dt>state</dt>
      <dd>{task.status.replace('_', ' ')}</dd>
      <dt>owner</dt>
      <dd>{task.owner === null ? 'unclaimed' : task.owner.label}</dd>
      <dt>estimate</dt>
      <dd>{task.estimate === null ? 'none' : task.estimate.words}</dd>
      <dt>worked</dt>
      <dd>{fmtSecs(row.worked_secs)}<span class="dim">{estimateWords}</span></dd>
      <dt>created</dt>
      <dd>{created}</dd>
      <dt>updated</dt>
      <dd>{updated} <span class="dim">{ago} ago</span></dd>
      <dt>parent</dt>
      <dd>
        {#if row.parent_subject !== null}
          <span class="b-chip blue b-epic-pill">&#8627; {row.parent_subject}</span>
        {:else}none{/if}
      </dd>
      {#if live}
        <dt>pick</dt>
        <dd>
          {#if task.reserved === 'user'}
            <span>reserved for you</span>
            <button
              type="button"
              class="dr-btn"
              onclick={() =>
                onact({
                  task_reserve: { project: task.project_name, id: task.id, reserved: false },
                })}>release</button
            >
          {:else}
            <button
              type="button"
              class="dr-btn"
              onclick={() =>
                onact({
                  task_reserve: { project: task.project_name, id: task.id, reserved: true },
                })}>reserve for you</button
            >
          {/if}
        </dd>
      {/if}
      <dt>links</dt>
      <dd>
        {#if task.links.length === 0}<span class="dim">none</span>{:else}
          <!-- Keyed on the position: the entries carry no identity the wire
               guarantees unique. -->
          {#each task.links as link, at (at)}
            {@const href = followable(link.target)}
            {#if at > 0}<span class="dim"> · </span>{/if}
            {#if href !== null}
              <a {href} target="_blank" rel="noreferrer">{link.label ?? link.target}</a>
            {:else}
              <span>{link.label ?? link.target}</span>
            {/if}
          {/each}
        {/if}
      </dd>
    </dl>

    {#if task.detail !== null && !detailIsAsk}
      <!-- When the wait states no ask of its own, the row's prose IS the ask
           in the section below; drawing it here too would say it twice. -->
      <p class="dr-detail">{task.detail}</p>
    {/if}

    {#if tasks.length > 0}
      <h3>Its tasks</h3>
      <div class="dr-kids">
        {#each tasks as child (child.id)}
          <button
            type="button"
            class="dr-kid"
            onclick={() => onjump?.(child.id)}
            aria-label="jump to {child.label}"
          >
            <span class="l">{child.label}</span>
            <span class="go" aria-hidden="true">&rarr;</span>
          </button>
        {/each}
      </div>
    {/if}

    {#if task.waiting_on !== null}
      <h3>
        {#if waitOnUser(task)}Waiting on you{:else}What it waits on{/if}
      </h3>
      <div class="dr-ask">
        <!-- The ASK leads: the wait's own detail where it states one, else
             the row's detail - a look parks with no wait detail, and its ask
             is the row's own words - else the state's own sentence. A seeded
             or legacy wait with no detail keeps that sentence. -->
        <div class="q">
          {#if task.waiting_on.detail !== null}
            {task.waiting_on.detail}
          {:else if task.detail !== null}
            {task.detail}
          {:else if waitOnUser(task) && task.waiting_on.verification}
            This row is finished and waits on your approval.
          {:else if waitOnUser(task) && task.waiting_on.kind === 'decision'}
            A decision is waiting on you.
          {:else if task.waiting_on.kind === 'dependency'}
            Waiting on {task.waiting_on.on ?? 'another row'}.
          {:else if task.waiting_on.kind === 'resource'}
            Waiting on a resource.
          {:else}
            Waiting.
          {/if}
        </div>
        {#if task.waiting_on.detail !== null || task.detail !== null}
          <!-- What the ask is, in the state's own words, under the ask itself. -->
          <div class="from">
            {#if waitOnUser(task) && task.waiting_on.verification}
              A look: this row is finished and waits on your approval.
            {:else if waitOnUser(task) && task.waiting_on.kind === 'decision'}
              A decision is waiting on you.
            {:else if task.waiting_on.kind === 'dependency'}
              Waiting on {task.waiting_on.on ?? 'another row'}.
            {:else if task.waiting_on.kind === 'resource'}
              Waiting on a resource.
            {:else}
              Waiting.
            {/if}
          </div>
        {/if}
        {#if refs.length > 0}
          <!-- The row's references are doors here: the ask usually needs one.
               The artifact the door below presents is left out of this line. -->
          <div class="from">
            {#each refs as link, at (at)}
              {#if at > 0}<span class="dim"> &middot; </span>{/if}
              {#if link.href !== null}
                <a class="b-ref" href={link.href} target="_blank" rel="noreferrer">{link.label}</a>
              {:else}
                <span>{link.label}</span>
              {/if}
            {/each}
          </div>
        {/if}
        {#if lookArtifact !== null}
          <div class="from">
            <!-- The artifact is a DOOR in the same grammar the board's wait
                 card uses, so the two surfaces read one control. -->
            <a class="b-door" href={lookArtifact.href} target="_blank" rel="noreferrer"
              >look at <span class="nm">{lookArtifact.label}</span></a
            >
          </div>
        {/if}
        {#if waitOnUser(task)}
          {#if task.waiting_on.verification}
            <div class="answer">
              {#if waiting !== null && waiting.root && waiting.open > 0}
                <!-- The core's own close rule: a root cannot complete while
                     any child is non-terminal, so an approve here is a press
                     the core refuses. Same words the strip uses. -->
                <span class="dr-hold"
                  >{waiting.rollup} done &middot; {waiting.open} not finished, and a root closes when
                  its children are</span
                >
              {:else}
                <button
                  type="button"
                  class="dr-btn go"
                  onclick={() =>
                    onact({
                      task_verdict: { project: task.project_name, id: task.id, approve: true },
                    })}>approve</button
                >
              {/if}
              <input
                data-editor="board"
                type="text"
                placeholder="what to change"
                aria-label="what to change (sent back with it)"
                bind:value={sendBack}
                onkeydown={(event: KeyboardEvent) => {
                  // **Enter is the box's own action** (Ved, 2026-10-15): the
                  // same key that sends everywhere sends this back, and an
                  // empty box ignores it as the button does.
                  if (event.key !== 'Enter' || event.shiftKey) return;
                  event.preventDefault();
                  if (sendBack.trim() === '') return;
                  onact({
                    task_verdict: {
                      project: task.project_name,
                      id: task.id,
                      approve: false,
                      words: sendBack,
                    },
                  });
                }}
              />
              <button
                type="button"
                class="dr-btn no"
                onclick={() =>
                  onact({
                    task_verdict: {
                      project: task.project_name,
                      id: task.id,
                      approve: false,
                      words: sendBack,
                    },
                  })}>send back</button
              >
            </div>
          {:else}
            <div class="answer">
              <input
                data-editor="board"
                type="text"
                placeholder="your answer"
                bind:value={answer}
                onkeydown={(event: KeyboardEvent) => {
                  // Enter answers, as the button does (Ved, 2026-10-15).
                  if (event.key !== 'Enter' || event.shiftKey) return;
                  event.preventDefault();
                  if (answer.trim() === '') return;
                  onact({
                    task_answer: { project: task.project_name, id: task.id, words: answer },
                  });
                }}
              />
              <button
                type="button"
                class="dr-btn go"
                onclick={() =>
                  onact({
                    task_answer: { project: task.project_name, id: task.id, words: answer },
                  })}>answer</button
              >
            </div>
          {/if}
        {/if}
        {#if seatHref !== null}
          <a class="dr-ses" href={seatHref}>or open the session &rarr;</a>
        {/if}
      </div>
    {/if}

    {#if task.attested != null}
      <h3>Lead-attested look</h3>
      <div class="dr-ask">
        <div class="q">The lead completed this row's look, citing your approval:</div>
        <div class="from">&ldquo;{task.attested.evidence}&rdquo;</div>
        <button
          type="button"
          class="dr-btn no"
          onclick={() => onact({ task_reopen: { project: task.project_name, id: task.id } })}
          >reopen</button
        >
      </div>
    {/if}

    <h3>History</h3>
    {#if withheld > 0}
      <!-- What the row's own carry withholds, said rather than silent: the
           board read caps a row's transitions so the whole trail does not
           ride every encode. -->
      <p class="dim">showing the last {row.history.length} of {row.history_total}</p>
    {/if}
    {#if row.history.length === 0}
      <p class="dr-none">no moves recorded</p>
    {:else}
      <div class="dr-hist">
        {#each row.history as step (`${step.at.secs_since_epoch}:${step.at.nanos_since_epoch}`)}
          <div class="h">
            <span class="w">{atWords(step.at.secs_since_epoch)}</span>
            <span class="what">
              {#if step.from === null}
                created by <b>{byWords(step.by)}</b>
              {:else}
                moved to <b>{movedWords(step.to)}</b> by {byWords(step.by)}
              {/if}
            </span>
          </div>
        {/each}
      </div>
    {/if}

    {#if task.owner !== null}
      <h3>From the seat</h3>
      <div class="dr-last">
        <span>{task.owner.label} holds this row · worked {fmtSecs(row.worked_secs)}</span>
        <a class="dr-ses" href={hrefForSlot(task.owner)}>open the session &rarr;</a>
      </div>
    {/if}
  </div>
</div>
