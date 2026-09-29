<script lang="ts">
  import { untrack } from 'svelte';

  import Icon from '../components/Icon.svelte';
  import Autocomplete from './Autocomplete.svelte';
  import Dictation from './Dictation.svelte';
  import Dock from './Dock.svelte';
  import { offer, type Sources } from './autocomplete';
  import { FORGE_COMMANDS } from './forge-commands';
  import {
    blocked,
    composerState,
    joined,
    noticeLine,
    pendingAsk,
    signInLine,
    type ComposerProps,
  } from './view';
  import { advisoriesFrom, agentTypesFrom, filesFrom } from './wire';

  /** How long a landed take's border holds its green beat, which the book states. */
  const BEAT_MS = 450;

  let {
    record,
    slot,
    connection,
    seat,
    // Off unless the page says otherwise: an install with `[dictate]` off loads
    // no models, and a control it cannot honour is worse than none.
    dictation = false,
  }: ComposerProps = $props();

  /**
   * The reader's own words, held HERE rather than in the field.
   *
   * The box morphs into the dock, so the field is unmounted while a prompt is
   * up - and a draft the field owned would go with it, which is the input-loss
   * defect this component's tests exist for. The dock takes the slot and the
   * draft waits here until the box comes back.
   */
  let draft = $state('');
  /** The slash command the reader sent and the turn is still working on. */
  let sent = $state<string | null>(null);
  /** The prompt this composer answered, while the core still lists it as waiting. */
  let answered = $state<string | null>(null);
  /** Why the core refused that answer, when it did. */
  let refusal = $state<string | null>(null);
  /**
   * The words a landed take has already put in the draft, so they land once.
   *
   * Deliberately not `$state`: nothing draws from it, and the effect below is
   * its only reader - a reactive copy would make that effect depend on what it
   * writes, so it would tear its own green beat down on the next run.
   */
  let landed: string | null = null;
  /** The line the reader's own typing has dismissed, which the next take clears. */
  let dismissed = $state<string | null>(null);
  /** One green beat while a take's words settle into the draft. */
  let beat = $state(false);
  /** The draft the reader closed the list at, which typing clears. */
  let closed = $state<string | null>(null);
  /** Which row a key would take, which is the first until one moves it. */
  let marked = $state(0);
  /** The field, so focus can go back to it when the box returns. */
  let field = $state<HTMLTextAreaElement | null>(null);

  const composer = $derived(composerState(record));
  const ask = $derived(pendingAsk(record));
  const blocker = $derived(blocked(seat, composer, sent));
  const filled = $derived(draft.trim() !== '');
  const notice = $derived(noticeLine(composer.notice));
  const line = $derived(notice !== null && dismissed === notice.text ? null : notice);

  const sources = $derived<Sources>({
    forgeCommands: FORGE_COMMANDS,
    advertised: advisoriesFrom(record.slash_commands),
    files: filesFrom(record.file_index),
    agents: agentTypesFrom(record.subagents),
  });
  const held = $derived(offer(draft, sources));
  const list = $derived(closed === draft ? null : held);

  // A new query is a new list, so a key starts at its first row again.
  $effect(() => {
    void draft;
    marked = 0;
  });

  // The reader's eye is in that slot: a prompt takes the box and the keyboard
  // with it, and the field takes it back when the box returns.
  $effect(() => {
    if (ask !== null) return;
    if (field !== null) field.focus();
  });

  /**
   * A landed take puts its words where the reader was about to type, then the
   * box takes one green beat.
   *
   * Tracked only on the notice: the draft is read through `untrack`, because an
   * effect that re-ran on the draft it writes would tear down its own timer and
   * leave the box green.
   */
  $effect(() => {
    const held = composer.notice;
    if (held === null) {
      landed = null;
      dismissed = null;
      return;
    }
    if (held.kind !== 'landed' || landed === held.text) return;
    landed = held.text;
    draft = joined(untrack(() => draft), held.text);
    beat = true;
    const timer = setTimeout(() => {
      beat = false;
    }, BEAT_MS);
    return () => clearTimeout(timer);
  });

  // A turn that has settled is no longer working on anything, so the line that
  // names what the reader sent goes with it.
  $effect(() => {
    if (!record.header.turn_in_flight) sent = null;
  });

  /**
   * The prompt changing is the answer landing.
   *
   * A refusal arrives while the prompt still waits, so a read that names a
   * different prompt - or none - is the news that the answer this composer sent
   * is not outstanding any more, and the line about it goes with the prompt.
   */
  $effect(() => {
    const current = ask;
    if (answered === null) return;
    if (current !== null && askToolId(current) === answered) return;
    answered = null;
    refusal = null;
  });

  // A command the socket refused. The composer only hears the ones it sent:
  // an answer that is still outstanding is the only one a refusal can be about,
  // because a page that moved on has already cleared its own.
  $effect(() => {
    return connection.onMessage((message) => {
      if (message.kind !== 'error' || message.what !== 'dispatch') return;
      if (answered === null) return;
      refusal = message.why;
    });
  });

  /** Send the draft, and remember the command when the draft was one. */
  function send(): void {
    const text = draft;
    if (text.trim() === '') return;
    try {
      connection.dispatch({ prompt: { key: slot, text, attachments: [] } });
    } catch (why) {
      // A closed socket throws rather than answering, and it is the one
      // channel left: the words stay in the box rather than going with a
      // command that never left the browser.
      return;
    }
    const [first = ''] = text.trim().split(/\s+/);
    if (first.startsWith('/')) sent = first;
    draft = '';
  }

  /**
   * The keys the field answers to, which are two surfaces in one slot: while a
   * list is open its keys win, and otherwise Enter sends.
   */
  function onkey(event: KeyboardEvent): void {
    if (list !== null && list.rows.length > 0) {
      if (event.key === 'ArrowDown') {
        event.preventDefault();
        marked = (marked + 1) % list.rows.length;
        return;
      }
      if (event.key === 'ArrowUp') {
        event.preventDefault();
        marked = (marked - 1 + list.rows.length) % list.rows.length;
        return;
      }
      if (event.key === 'Escape') {
        event.preventDefault();
        closed = draft;
        return;
      }
      if (event.key === 'Enter') {
        event.preventDefault();
        pick(marked);
        return;
      }
    }
    if (event.key !== 'Enter' || event.shiftKey) return;
    event.preventDefault();
    send();
  }

  /** Write the marked row into the draft, replacing the token it opened on. */
  function pick(at: number): void {
    const row = list?.rows[at];
    if (list === null || row === undefined) return;
    draft = `${draft.slice(0, list.from)}${row.insert} `;
    closed = null;
    // The click landed on the row, so the field takes the keyboard back with
    // the words - the reader is typing again rather than having chosen a button.
    field?.focus();
  }

  /** The reader's own typing is what dismisses a notice row. */
  function oninput(): void {
    dismissed = notice?.text ?? null;
  }

  /**
   * The way into a take, and the way to submit the one that is running: the key
   * that opens a take is the key that closes it, which is the terminal's rule.
   */
  function mic(): void {
    if (composer.take === null) {
      connection.dispatch({ dictate_start: { key: slot } });
      return;
    }
    connection.dispatch({ dictate_stop: { key: slot, submit: true } });
  }

  /** The tool the prompt is waiting on, which is how an answer is told apart from the next one. */
  function askToolId(current: ReturnType<typeof pendingAsk>): string | null {
    if (current === null) return null;
    return current.kind === 'permission' || current.kind === 'question'
      ? current.request.toolId
      : null;
  }
</script>

{#if blocker !== null}
  <div class="comp">
    <div class="box" class:err={blocker.bad}>
      <div class="blocked">
        <span class="b1">
          {#if blocker.waiting}<span class="ring"></span>{/if}
          {blocker.line}
        </span>
        {#if blocker.sub !== null}<span class="b2">{blocker.sub}</span>{/if}
      </div>
    </div>
  </div>
{:else if ask !== null}
  <div class="comp">
    <Dock
      {ask}
      {slot}
      {connection}
      depth={seat.pendingDepth}
      notice={refusal}
      onanswer={(toolId) => (answered = toolId)}
    />
  </div>
{:else}
  <div class="comp">
    {#if seat.lifecycle === 'AuthRequired'}
      <div class="hint login">
        Authentication required{#if composer.signIn !== null && composer.signIn.methodName !== ''}
          · {composer.signIn.methodName}{/if}
        <span class="sub">{signInLine(composer)}</span>
      </div>
    {/if}
    <div
      class="box"
      class:rec={composer.take?.phase === 'recording'}
      class:tr={composer.take?.phase === 'transcribing'}
      class:done={beat}
    >
      {#if composer.take !== null}
        <Dictation take={composer.take} {slot} {connection} />
      {:else if line !== null}
        <div class="notice {line.tone}">{line.text}</div>
      {/if}
      {#if list !== null}
        <Autocomplete {list} offer={list} {marked} onpick={pick} />
      {/if}
      <div class="line">
        <textarea
          class="txt"
          name="draft"
          autocomplete="off"
          spellcheck="false"
          placeholder="Type a message…"
          bind:this={field}
          bind:value={draft}
          oninput={oninput}
          onkeydown={onkey}
        ></textarea>
        {#if filled}
          <button class="send" type="button" title="send" aria-label="send" onclick={send}>
            <Icon name="send" />
          </button>
        {/if}
      </div>
      {#if filled || dictation}
        <div class="foot">
          {#if filled}
            <span><kbd>Shift</kbd> <kbd>Enter</kbd> newline</span>
            <span><kbd>Enter</kbd> send</span>
          {/if}
          {#if dictation}
            <button
              class="mic"
              type="button"
              aria-label={composer.take === null ? 'start a take' : 'submit the take'}
              onclick={mic}
            >
              <Icon name="mic" />
            </button>
          {/if}
        </div>
      {/if}
    </div>
  </div>
{/if}
