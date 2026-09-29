<script lang="ts">
  import Icon from '../components/Icon.svelte';
  import Dock from './Dock.svelte';
  import { blocked, composerState, pendingAsk, type ComposerProps } from './view';

  let { record, slot, connection, seat }: ComposerProps = $props();

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

  const composer = $derived(composerState(record));
  const ask = $derived(pendingAsk(record));
  const blocker = $derived(blocked(seat, composer, sent));
  const filled = $derived(draft.trim() !== '');

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

  /** Enter sends, Shift+Enter is a newline - the keys the box's own foot names. */
  function onkey(event: KeyboardEvent): void {
    if (event.key !== 'Enter' || event.shiftKey) return;
    event.preventDefault();
    send();
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
    <div class="box">
      <div class="line">
        <textarea
          class="txt"
          name="draft"
          autocomplete="off"
          spellcheck="false"
          placeholder="Type a message…"
          bind:value={draft}
          onkeydown={onkey}
        ></textarea>
        {#if filled}
          <button class="send" type="button" title="send" aria-label="send" onclick={send}>
            <Icon name="send" />
          </button>
        {/if}
      </div>
      {#if filled}
        <div class="foot">
          <span><kbd>Shift</kbd> <kbd>Enter</kbd> newline</span>
          <span><kbd>Enter</kbd> send</span>
        </div>
      {/if}
    </div>
  </div>
{/if}
