<script lang="ts">
  import { untrack } from 'svelte';

  import Icon from '../components/Icon.svelte';
  import Autocomplete from './Autocomplete.svelte';
  import Dictation from './Dictation.svelte';
  import DictationPanel from './DictationPanel.svelte';
  import Dock from './Dock.svelte';
  import Field from './Field.svelte';
  import { LIST_ID, offer, rowId, type Sources } from './autocomplete';
  import {
    boundCode,
    down,
    isBareModifier,
    markChorded,
    up,
    type Action,
    type Held,
  } from './dictate-key';
  import { devicePick } from './dictation';
  import { focusOf, type Where } from './editors';
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

  let { record, slot, connection, seat, dictation, device = null }: ComposerProps = $props();

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
   * The words a send is waiting on, while the core has not yet taken them.
   *
   * A send is fire-and-forget and the box is cleared for the next thing, so
   * without this the words are gone before a refusal can arrive - and a prompt
   * the core refused would take the reader's typed message with it.
   */
  let sending = $state<string | null>(null);
  /** Why the core refused a send, which the box draws until the reader types. */
  let bounced = $state<string | null>(null);
  /**
   * The words a landed take has already put in the draft, so they land once.
   *
   * Deliberately not `$state`: nothing draws from it, and the effect below is
   * its only reader - a reactive copy would make that effect depend on what it
   * writes, so it would tear its own green beat down on the next run.
   */
  let landed: string | null = null;
  /**
   * Whether this composer has seen the seat hold a take, ever.
   *
   * A landed notice is per-seat server state that outlives its take, so a
   * client that attaches - or reloads - finds one whose words it never saw
   * land. Only a take this composer saw may put words in the box, and the
   * trade is that dictation landing unwatched, and never sent, is not handed
   * back.
   */
  let sawTake = false;
  /** The line the reader's own typing has dismissed, which the next take clears. */
  let dismissed = $state<string | null>(null);
  /** One green beat while a take's words settle into the draft. */
  let beat = $state(false);
  /** The draft the reader closed the list at, which typing clears. */
  let closed = $state<string | null>(null);
  /** Which row a key would take, which is the first until one moves it. */
  let marked = $state(0);
  /** The field, so focus can go back to it when the box returns. */
  let field = $state<HTMLElement | null>(null);
  /** Whether the dictation panel is showing, which the mic opens. */
  let panel = $state(false);
  /** The bound key's press in flight, from its press to its release. */
  let pressed: Held | null = null;
  /** Whether this platform delivers Cmd, which is what a binding names. */
  const mac = navigator.platform.toLowerCase().includes('mac');

  const composer = $derived(composerState(record));
  const ask = $derived(pendingAsk(record));

  /** What the reader wrote in the dock's box, which a landed take has to reach. */
  let dockDraft = $state('');
  /** Whether the dock has its own box open, which is what makes it a destination. */
  let dockOpen = $state(false);
  /** A take's words that landed in the dock, which is what brings its box the keyboard. */
  let dockLanded = $state<string | null>(null);

  /**
   * What the table needs to say which box holds the keyboard.
   *
   * This is the session route's box, so the connect screen's is never up beside
   * it - and this one is not drawn at all while a prompt has the slot, because
   * the dock morphs it.
   */
  const where = $derived({
    editor: 'composer',
    remember: 'composer',
    pending: ask !== null,
    composerPresent: ask === null,
    connectPresent: false,
    dockPresent: dockOpen,
  } satisfies Where);

  const blocker = $derived(blocked(seat, composer, sent));
  const filled = $derived(draft.trim() !== '');
  const notice = $derived(noticeLine(composer.notice, sawTake));
  const line = $derived(notice !== null && dismissed === notice.text ? null : notice);

  /**
   * The lists a draft is matched against, pulled when a list is opened rather
   * than on every frame.
   *
   * All four are built together on the pull, which is what keeps `held`'s
   * dependencies equal to the sources it reads. What the pull saves is the
   * record being replaced per frame, so a draft that opens nothing never
   * rebuilds a list it will not draw.
   */
  function sources(): Sources {
    return {
      forgeCommands: FORGE_COMMANDS,
      advertised: advisoriesFrom(record.slash_commands),
      files: filesFrom(record.file_index),
      agents: agentTypesFrom(record.subagents),
    };
  }
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

  // Holding the seat's take is having seen it, and the flag never clears.
  $effect(() => {
    if (composer.take !== null) sawTake = true;
  });

  /** What the dock's own box belongs to, which is what its words go with. */
  let ownKey: string | null = null;

  // The dock's box belongs to the prompt, so what was written in it goes when
  // the prompt does - which is what the dock's own mount used to do for it.
  //
  // Keyed on the prompt's identity rather than on its absence: the next prompt
  // can arrive in the same frame as the last, and a release that only fires on
  // `ask === null` never sees that, so the words would come back in the box
  // that replaced them.
  $effect(() => {
    const key = ownKeyOf(ask);
    if (key === ownKey) return;
    ownKey = key;
    dockDraft = '';
    dockLanded = null;
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
    if (held.kind !== 'landed' || !sawTake || landed === held.text) return;
    landed = held.text;
    // Where the words go is the table's answer rather than this component's:
    // while a prompt has the slot the reader's box is the dock's, and this one
    // is not drawn at all.
    if (untrack(() => focusOf(where)) === 'dock') {
      dockDraft = joined(
        untrack(() => dockDraft),
        held.text,
      );
      dockLanded = held.text;
    } else {
      draft = joined(
        untrack(() => draft),
        held.text,
      );
    }
    beat = true;
    // The words come with the keyboard, so an immediate Enter sends what just
    // landed. The guards above already make this the landing rather than every
    // frame. When the words went to the dock this handle still holds the element
    // the prompt replaced - destroyed with the box, so detached, and taking no
    // focus - and the dock brings its own box back from its `land`.
    field?.focus();
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

  // A turn in flight is the send landing: the words are the core's now, so the
  // box owes the reader nothing back.
  $effect(() => {
    if (record.header.turn_in_flight) sending = null;
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

  /**
   * A command the socket refused, which is a refusal about something this
   * composer sent and is still waiting on.
   *
   * An answer counts while its prompt is still the core's, and a send counts
   * until the turn it started is in flight - so a refusal that arrives while
   * neither is outstanding belongs to another page, and drawing it here would
   * put a stranger's failure in the reader's box.
   */
  $effect(() => {
    return connection.onMessage((message) => {
      if (message.kind !== 'error' || message.what !== 'dispatch') return;
      if (answered !== null) {
        refusal = message.why;
        return;
      }
      if (sending === null) return;
      // The words come back with the reason: a send the core refused took the
      // box's text with it, and losing it is the defect this guards.
      draft = sending;
      sending = null;
      bounced = message.why;
    });
  });

  /** Send the draft, and remember the command when the draft was one. */
  function send(): void {
    // Trimmed at the ends like the terminal's own submit: picking a row off the
    // list leaves a trailing space for the next word, and an answer that goes
    // out as `/compact ` is one nothing asked for.
    const text = draft.trim();
    if (text === '') return;
    try {
      // A prompt is fire-and-forget: its outcome rides the subscription rather
      // than a reply, so there is nothing here to await.
      void connection.dispatch({ prompt: { key: slot, text, attachments: [] } });
    } catch {
      // A closed socket throws rather than answering, and it is the one
      // channel left: the words stay in the box rather than going with a
      // command that never left the browser.
      return;
    }
    const [first = ''] = text.split(/\s+/);
    if (first.startsWith('/')) sent = first;
    sending = text;
    bounced = null;
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
    bounced = null;
  }

  /** What the bound key asks for, which is the terminal's own three. */
  function act(action: Action): void {
    if (action === 'begin') {
      void connection.dispatch({ dictate_start: { key: slot } });
      return;
    }
    void connection.dispatch({ dictate_stop: { key: slot, submit: action === 'finish' } });
  }

  /**
   * The push-to-talk key, which is how a take begins and ends.
   *
   * Registered on the window rather than on the field: the binding is a bare
   * modifier, so it arrives wherever the focus happens to be, and the
   * terminal's own handler is global for the same reason. What the key means
   * is read when one arrives rather than here, so this registers once instead
   * of re-registering on every frame the record moves.
   */
  $effect(() => {
    if (!dictation) return;
    const onDown = (event: KeyboardEvent): void => {
      // A held key repeats, and a repeat carries no instruction - which is the
      // terminal's own rule, and without it a toggle take stops once per repeat.
      if (event.repeat) return;
      if (event.key === 'Escape') {
        // A live take consumes Esc, which is the terminal's rule: the surfaces
        // under it never see the key, so one press is one command and the list
        // a field would close stays where it is.
        if (composer.take !== null) {
          event.preventDefault();
          event.stopPropagation();
          act('cancel');
        } else if (panel) {
          // The next Escape belongs to the panel, which is the surface that is
          // open rather than a take that is not.
          event.preventDefault();
          event.stopPropagation();
          panel = false;
          field?.focus();
        }
        return;
      }
      if (event.code !== boundCode(composer.bind, mac)) {
        // Any other KEY while the press is down makes it a chord, and the
        // chord's release discards what the press began. A bare modifier is
        // not a key: the terminal lets those through unchorded.
        if (!isBareModifier(event.key)) pressed = markChorded(pressed);
        return;
      }
      const step = down(pressed, composer.take !== null, Date.now(), composer.mode);
      pressed = step.held;
      if (step.action !== null) act(step.action);
    };
    const onUp = (event: KeyboardEvent): void => {
      if (event.code !== boundCode(composer.bind, mac)) return;
      const step = up(pressed, Date.now(), composer.mode);
      pressed = step.held;
      if (step.action !== null) act(step.action);
    };
    window.addEventListener('keydown', onDown, true);
    window.addEventListener('keyup', onUp);
    return () => {
      window.removeEventListener('keydown', onDown, true);
      window.removeEventListener('keyup', onUp);
    };
  });

  /**
   * The mic is the door, not the trigger: pressing it shows what dictation is
   * set to, and nothing on the page starts a take. The push-to-talk key is the
   * trigger, which is the terminal's own shape - it has no record button
   * either.
   */
  function mic(): void {
    panel = !panel;
  }

  /** Abandon a take without submitting it, which the dock's Escape does. */
  function abandon(): void {
    void connection.dispatch({ dictate_stop: { key: slot, submit: false } });
  }

  /** Remember which prompt this reader answered, while the core still lists it. */
  function remember(toolId: string | null): void {
    answered = toolId;
  }

  /**
   * What a prompt's own-words box belongs to: one prompt, not one tool call.
   *
   * A batch of questions rides one tool call - the core reuses its id and
   * advances only the index - so the id alone would leave question one's words
   * in the box question two opens. Not `askToolId`, which backs the comparison
   * that tells an answered prompt from the next and names what the dock
   * dispatches under.
   *
   * A held Slack post carries no id of its own and keys as null, so two in a row
   * share one. Nothing stateful rides on that dock - it draws a head and a
   * description and no rows at all - so a shared key has nothing to carry. Give
   * it rows and this key needs a field for it, the way a question's has one for
   * its index.
   */
  function ownKeyOf(current: ReturnType<typeof pendingAsk>): string | null {
    if (current === null) return null;
    if (current.kind === 'permission') return `permission:${current.request.toolId}`;
    if (current.kind === 'question') {
      return `question:${current.request.toolId}:${String(current.request.index)}`;
    }
    return null;
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
    <!-- Keyed on the prompt, so a batch's next question is a fresh dock: the
         wire's option ids are positional, so a mark or a toggle left over from
         the question before is a valid answer to the one after, and the core
         accepts it. A repaint of the same prompt keeps its key, so a frame
         arriving clears nothing the reader has turned on. -->
    {#key ownKeyOf(ask)}
      <Dock
        {ask}
        {slot}
        {connection}
        depth={seat.pendingDepth}
        notice={refusal}
        take={composer.take}
        bind:notes={dockDraft}
        bind:ownOpen={dockOpen}
        land={dockLanded}
        onanswer={remember}
        onabandon={abandon}
      />
    {/key}
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
      {:else if bounced !== null}
        <div class="notice bad">{bounced}</div>
      {:else if line !== null}
        <div class="notice {line.tone}">{line.text}</div>
      {/if}
      {#if list !== null}
        <Autocomplete offer={list} {marked} onpick={pick} />
      {/if}
      <div class="line">
        <!-- The list belongs to the field rather than being a surface the reader
             Tabs into: the field keeps focus and points at the marked row. The
             combobox role is not available here - ARIA allows it on an input,
             not on a textarea - so this is the textbox-and-controlled-listbox
             shape, which is what a multi-line composer can validly be. -->
        <Field
          editor="composer"
          class="txt"
          name="draft"
          placeholder="Type a message..."
          aria={{
            autocomplete: 'list',
            controls: list === null ? undefined : LIST_ID,
            activeDescendant: list === null ? undefined : rowId(list, marked),
          }}
          bind:value={draft}
          {oninput}
          onkeydown={onkey}
          field={(el: HTMLElement | null) => {
            field = el;
          }}
        />
        {#if dictation}
          <button
            class="mic"
            type="button"
            aria-label="dictation settings"
            aria-expanded={panel}
            onclick={mic}
          >
            <Icon name="mic" />
          </button>
        {/if}
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
    {#if panel}
      <DictationPanel
        overrides={record.dictate_overrides}
        bind={composer.bind}
        mode={composer.mode}
        {slot}
        {connection}
        device={devicePick(device)}
      />
    {/if}
  </div>
{/if}
