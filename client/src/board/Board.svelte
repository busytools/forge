<script lang="ts">
  import { hrefForSlot } from '../routes';
  import type { HomeWire } from '../wire/home';
  import { boardView } from './view';

  /**
   * One project's board, opened as a takeover: a slim top bar with back and
   * done, the rows waiting on the reader, the ranked rows two levels deep,
   * and the controls that move them. Nothing of any other project appears
   * here - the fleet is the glance, this is the world.
   *
   * The page is a view: every edit it offers is handed out through `onact`,
   * which the app wires to the socket's command path. Without one the
   * controls still draw - the page's shape does not depend on a live
   * connection - and a press does nothing.
   */
  let {
    wire,
    org,
    project,
    onact = null,
  }: {
    wire: HomeWire;
    org: string;
    project: string;
    onact?: ((command: Record<string, Record<string, unknown>>) => void) | null;
  } = $props();

  const view = $derived(boardView(wire, org, project));
  /** The project's seats, for the owner picker: the label that may hold a row. */
  const seats = $derived(
    wire.agents
      .filter((agent) => agent.slot.org === org && agent.slot.project === project)
      .map((agent) => ({ label: agent.label, href: hrefForSlot(agent.slot) })),
  );
  /** The epics a new task can nest under. */
  const epics = $derived(view.rows.filter((row) => row.depth === 0));
  /** The worker's own words on a row it sent back, before it is dispatched. */
  const sendBackWords: Record<string, string> = $state({});
  const answers: Record<string, string> = $state({});
  let subject = $state('');
  let parent = $state('');

  function act(command: Record<string, Record<string, unknown>>): void {
    onact?.(command);
  }

  /** Close the takeover: back the way the reader came, or home from a deep link. */
  function close(): void {
    if (history.length > 1) history.back();
    else location.assign('/');
  }

  /** The status mark's class, one per state - shape and colour both. */
  function markClass(status: string): string {
    switch (status) {
      case 'in_progress':
        return 'b-run';
      case 'waiting':
        return 'b-wait';
      case 'completed':
        return 'b-done';
      case 'failed':
        return 'b-fail';
      case 'canceled':
        return 'b-cancel';
      default:
        return 'b-pend';
    }
  }
</script>

<main class="wrap board">
  <div class="b-top">
    <button type="button" class="b-back" onclick={close}>{'\u{2190}'} back</button>
    <span class="b-nm">{project}</span>
    <button type="button" class="b-done" onclick={close}>done</button>
  </div>

  {#if seats.length > 0}
    <p class="b-seats">
      {#each seats as seat (seat.label)}
        <a class="b-seat" href={seat.href}>{seat.label}</a>
      {/each}
    </p>
  {/if}

  {#if view.waiting.length > 0}
    <section class="b-you" aria-label="waiting on you">
      <h2>waiting on you</h2>
      {#each view.waiting as row (row.id)}
        <div class="b-wrow">
          <span class="b-wnm">{row.subject}</span>
          {#if row.owner !== null}<span class="b-owner">{row.owner}</span>{/if}
          <span class="b-age">{row.age}</span>
          {#if row.verification}
            <span class="b-facts">{row.detail ?? 'your look'}</span>
            <span class="b-acts">
              <button
                type="button"
                class="b-btn ok"
                onclick={() => act({ task_verdict: { project, id: row.id, approve: true } })}
                >approve</button
              >
              <input
                class="b-in"
                data-editor="board"
                type="text"
                placeholder="what to change (sent back with it)"
                bind:value={sendBackWords[row.id]}
              />
              <button
                type="button"
                class="b-btn no"
                onclick={() =>
                  act({
                    task_verdict: {
                      project,
                      id: row.id,
                      approve: false,
                      words: sendBackWords[row.id] ?? '',
                    },
                  })}>send back</button
              >
            </span>
          {:else}
            <span class="b-facts">{row.detail ?? 'a question for you'}</span>
            <span class="b-acts">
              <input
                class="b-in"
                data-editor="board"
                type="text"
                placeholder="your answer"
                bind:value={answers[row.id]}
              />
              <button
                type="button"
                class="b-btn"
                onclick={() =>
                  act({ task_answer: { project, id: row.id, words: answers[row.id] ?? '' } })}
                >answer</button
              >
            </span>
          {/if}
        </div>
      {/each}
    </section>
  {/if}

  {#if view.empty}
    <p class="b-empty">
      nothing owned, nothing waiting - cut an epic below, or tell the lead what is next
    </p>
  {/if}

  <ul class="b-rows">
    {#each view.rows as row (row.id)}
      <li class="b-row {row.depth === 1 ? 'b-child' : ''}">
        <span class="b-mark {markClass(row.status)}" aria-hidden="true"></span>
        <span class="b-subject" class:settled={row.status === 'completed'}>{row.display}</span>
        {#if row.owner !== null}<span class="b-owner">{row.owner}</span>{/if}
        <span class="b-num">
          worked {row.worked}{#if row.estimate !== null}{' \u{b7} '}est {row.estimate}{/if}
        </span>
        <span class="b-chips">
          {#if row.rollup !== null}<span class="b-chip">{row.rollup} done</span>{/if}
          {#each row.chips as chip (chip.label)}
            <span class="b-chip {chip.tone}">{chip.label}</span>
          {/each}
        </span>
        <span class="b-links">
          {#each row.links as link (link.label)}
            {#if link.href !== null}
              <a href={link.href} target="_blank" rel="noreferrer">{link.label}</a>
            {:else}
              <span class="b-link">{link.label}</span>
            {/if}
          {/each}
        </span>
        <span class="b-acts">
          <button
            type="button"
            class="b-btn"
            title="move to the top"
            onclick={() => act({ task_rank: { project, id: row.id, to: 'top' } })}
            >{'\u{2912}'}</button
          >
          <button
            type="button"
            class="b-btn"
            title="move up"
            onclick={() => act({ task_rank: { project, id: row.id, to: 'up' } })}
            >{'\u{2191}'}</button
          >
          <button
            type="button"
            class="b-btn"
            title="move down"
            onclick={() => act({ task_rank: { project, id: row.id, to: 'down' } })}
            >{'\u{2193}'}</button
          >
          <select
            class="b-owner-pick"
            aria-label="the seat holding {row.subject}"
            value={row.owner ?? ''}
            onchange={(event) =>
              act({
                task_assign: {
                  project,
                  id: row.id,
                  owner: event.currentTarget.value === '' ? null : event.currentTarget.value,
                },
              })}
          >
            <option value="">unclaimed</option>
            {#each seats as seat (seat.label)}
              <option value={seat.label}>{seat.label}</option>
            {/each}
          </select>
        </span>
      </li>
    {/each}
  </ul>

  <div class="b-create">
    <input
      class="b-in"
      data-editor="board"
      type="text"
      placeholder="a new row"
      bind:value={subject}
    />
    <select class="b-owner-pick" aria-label="the epic it belongs under" bind:value={parent}>
      <option value="">no epic</option>
      {#each epics as epic (epic.id)}
        <option value={epic.id}>{epic.subject}</option>
      {/each}
    </select>
    <button
      type="button"
      class="b-btn"
      onclick={() => {
        act({
          task_create: {
            project,
            subject,
            parent: parent === '' ? null : parent,
          },
        });
        subject = '';
      }}>+ add</button
    >
  </div>
</main>
