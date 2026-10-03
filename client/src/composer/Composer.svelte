<script lang="ts">
  import { untrack } from 'svelte';

  import { echoes } from '../chat/echoes.svelte';
  import Icon from '../components/Icon.svelte';
  import { slotOf } from '../protocol';
  import { variantOf } from '../session/apply';
  import { report } from '../socket';
  import { Boxes, boxKey, type Box } from './box.svelte';
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
    draftEndingLine,
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
   * Every box this composer has opened, held for as long as it lives: leaving a
   * seat keeps its box and coming back hands the same one over.
   */
  const boxes = new Boxes();

  /**
   * The box for the seat showing now.
   *
   * The state is the SEAT's rather than this component's, because the page
   * hands this composer one seat's record after another and keeps it mounted
   * across the move. The reader's own words are held HERE rather than in the
   * field: the box morphs into the dock, so the field is unmounted while a
   * prompt is up, and a draft the field owned would go with it - the
   * input-loss defect this component's tests exist for.
   *
   * **A pointer this component moves, rather than a value derived from
   * `slot`**, for two reasons:
   *
   * - `Boxes` mints a seat's box on first use, and a derivation that resolved
   *   one would mint from inside a derivation - a write Svelte refuses with
   *   `state_unsafe_mutation` rather than drawing the box at all.
   * - The swap has to land before the effects that write to the box: the seat
   *   change and the record that belongs to it arrive in one flush, and a
   *   landing that record carries belongs to the seat being moved TO. That is
   *   an order rather than a position, which is why the move is in
   *   `$effect.pre` - a plain effect would be early only while it stayed
   *   declared above them.
   */
  // The seat being mounted on is the right initial value: the effect below
  // takes it from there, and this is the one read that is not a re-render.
  // svelte-ignore state_referenced_locally
  let box = $state.raw<Box>(boxes.of(boxKey(slot)));
  $effect.pre(() => {
    const key = boxKey(slot);
    const next = boxes.of(key);
    if (untrack(() => box) !== next) box = next;
  });

  /**
   * Whether the record in hand is the shown seat's.
   *
   * The page keeps this composer mounted while it hands it one seat's record
   * after another, and between a switch and the new seat's first record what
   * it hands over is the seat being LEFT's - seconds, on a seat being read for
   * the first time. Everything a record WRITES is gated on this, because a
   * record's writes are the seat's own: a landing the left seat's record still
   * carries must not land in the box that just moved.
   */
  const owns = $derived(boxKey(record.slot) === boxKey(slot));

  /** The clock the beat's window is read against, which the close below moves. */
  let clock = $state(Date.now());
  /** The field, so focus can go back to it when the box returns. */
  let field = $state<HTMLElement | null>(null);
  /** Whether the dictation panel is showing, which the mic opens. */
  let panel = $state(false);
  /** The bound key's press in flight, from its press to its release. */
  let pressed: Held | null = null;
  /** Whether this platform delivers Cmd, which is what a binding names. */
  const mac = navigator.platform.toLowerCase().includes('mac');

  const composer = $derived(composerState(record));
  // A prompt only the shown seat's record may put up: answering the seat being
  // left's prompt through this composer would dispatch it as the seat moved to.
  const ask = $derived(owns ? pendingAsk(record) : null);
  /** Whether the landed beat's window is still open. */
  const beat = $derived(box.beatAt !== null && clock - box.beatAt < BEAT_MS);

  /**
   * What the ring is doing: one state rather than three that can overlap.
   *
   * A live take owns the ring, and the landed beat reaches it only when no take
   * does.
   */
  const ring = $derived.by(() => {
    if (composer.take !== null) return composer.take.phase === 'recording' ? 'rec' : 'tr';
    return beat ? 'done' : null;
  });

  /** What the reader wrote in the dock's box, which a landed take has to reach. */
  let dockDraft = $state('');
  /** Whether the dock has its own box open, which is what makes it a destination. */
  let dockOpen = $state(false);
  /** A take's words that landed in the dock, which is what brings its box the keyboard. */
  let dockLanded = $state<string | null>(null);

  /**
   * The prompt the dock draws, which is the one the seat is parked on - unless
   * this composer has answered a held draft.
   *
   * A question's answer clears the ask with an update of its own. A draft's
   * leaves the core's registry, and the stand-down that says so is a round trip
   * away - so the mark stands the dock down from the click until the update
   * lands, and a refusal brings it back with the reason.
   */
  const dockAsk = $derived(
    ask !== null &&
      !(ask.kind === 'slack_draft' && box.answered === ask.request.id && box.refusal === null)
      ? ask
      : null,
  );

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
    pending: dockAsk !== null,
    composerPresent: dockAsk === null,
    connectPresent: false,
    dockPresent: dockOpen,
  } satisfies Where);

  const blocker = $derived(blocked(seat, composer, box.sent));
  const filled = $derived(box.draft.trim() !== '');

  /**
   * Whether this seat has a turn to stop.
   *
   * Read from the header rather than from anything this composer sent, because
   * a turn started anywhere - another page, a cron, a delivery - is the same
   * turn to stop, and the core is the one that knows it is running.
   */
  const running = $derived(record.header.turn_in_flight);
  // The engine's notice, or what became of a draft that left this box - the
  // dock's own stand-down, said in the row the dock leaves behind.
  const notice = $derived(owns ? (noticeLine(composer.notice, box.sawTake) ?? box.ended) : null);
  const line = $derived(notice !== null && box.dismissed === notice.text ? null : notice);

  /**
   * The draft this box is drawing, remembered so a stand-down can tell THIS
   * draft from the next one: the record has already lost `pending_ask` by the
   * time the update is read.
   */
  $effect(() => {
    const held = ask;
    if (held !== null && held.kind === 'slack_draft') {
      box.shownDraft = held.request.id;
      box.ended = null;
    }
  });

  /**
   * A held draft leaving the core, which no record field carries.
   *
   * Applying the update clears `pending_ask`; the ENDING rides the update
   * alone, and it is what tells this reader what happened to a dock they did
   * not answer. The update is read here for the same reason the conversation
   * reads its own frames: nothing else draws it.
   *
   * It is recorded into the box for the update's OWN seat rather than the one
   * on screen: a reader looking elsewhere still meets the line when they come
   * back, and a seat nothing has drawn has no dock whose loss needs saying.
   */
  $effect(() => {
    return connection.onMessage((message) => {
      if (message.kind !== 'update') return;
      const [name, payload] = variantOf(message.update);
      if (name !== 'slack_draft_resolved') return;
      const at = slotOf(message.update);
      if (at === null) return;
      const held = boxes.held(boxKey(at));
      if (held === undefined) return;
      const id = typeof payload['id'] === 'string' ? payload['id'] : null;
      if (id === null || id !== held.shownDraft) return;
      // The reader's own answer, taken or not: the refusal that follows says
      // so when it is not, and the ending would only repeat the click.
      if (held.answered === id) return;
      held.ended = draftEndingLine(payload['ending']);
    });
  });

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
  const held = $derived(offer(box.draft, sources));
  const list = $derived(box.closed === box.draft ? null : held);

  // A new query is a new list, so a key starts at its first row again.
  $effect(() => {
    void box.draft;
    box.marked = 0;
  });

  // The reader's eye is in that slot: a prompt takes the box and the keyboard
  // with it, and the field takes it back when the box returns.
  $effect(() => {
    if (ask !== null) return;
    if (field !== null) field.focus();
  });

  // Holding the seat's take is having seen it, and the flag never clears -
  // and only the seat's own record arms it: another seat's take, still in the
  // record in hand between two seats, is not this box having watched anything.
  $effect(() => {
    if (owns && composer.take !== null) box.sawTake = true;
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
   * Tracked on the notice alone: the draft is read through `untrack`, so what
   * this effect writes cannot re-run it.
   */
  $effect(() => {
    if (!owns) return;
    const held = composer.notice;
    if (held === null) {
      box.landed = null;
      box.dismissed = null;
      return;
    }
    if (held.kind !== 'landed' || !box.sawTake || box.landed === held.text) return;
    box.landed = held.text;
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
      box.draft = joined(
        untrack(() => box.draft),
        held.text,
      );
    }
    box.beatAt = Date.now();
    // The words come with the keyboard, so an immediate Enter sends what just
    // landed. The guards above already make this the landing rather than every
    // frame. When the words went to the dock this handle still holds the element
    // the prompt replaced - destroyed with the box, so detached, and taking no
    // focus - and the dock brings its own box back from its `land`.
    field?.focus();
  });

  /**
   * Close the beat's window, which is the one repaint it owes.
   *
   * The window is a comparison and this only its schedule: every run arms from
   * the landing's own instant, and the write is the deadline itself, so an
   * early fire still closes the window.
   */
  $effect(() => {
    const at = box.beatAt;
    if (at === null) return;
    const left = at + BEAT_MS - Date.now();
    const timer = setTimeout(() => {
      clock = at + BEAT_MS;
    }, left);
    return () => clearTimeout(timer);
  });

  // A turn that has settled is no longer working on anything, so the line that
  // names what the reader sent goes with it.
  $effect(() => {
    if (!record.header.turn_in_flight) box.sent = null;
  });

  // A turn going in flight is the core taking the send: the mark that says it
  // has not come off, and the words stay on screen until the conversation
  // carries its own copy of them (the column clears the echo on those).
  //
  // Gated on `owns`, like the landing above it: between a switch and the new
  // seat's first record the record in hand is the seat being left's, and its
  // turn says nothing about a send on the seat being shown. `take` itself
  // refuses a send posted into a turn already running, so this fires only for
  // a send the turn it names actually started.
  $effect(() => {
    if (!owns) return;
    if (record.header.turn_in_flight) echoes.take(boxKey(slot));
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
    if (box.answered === null) return;
    if (current !== null && askToolId(current) === box.answered) return;
    box.answered = null;
    box.answeredKey = null;
    box.refusal = null;
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
      if (message.kind !== 'error') return;
      // The answer to a draft the core no longer holds, refused by its own
      // operation's name: the dock is gone by then, so the reason is drawn
      // where it stood. A generic `dispatch` refusal cannot say which command
      // it was about, and would be read as the dock's own.
      if (message.what === 'respond_slack_post') {
        box.ended = { tone: 'warn', text: message.why };
        return;
      }
      if (message.what !== 'dispatch') return;
      if (box.answered !== null) {
        box.refusal = message.why;
        return;
      }
      // The words stay where they are and the reason is named beside them: a
      // refusal that put the text back in the box landed it over whatever the
      // reader had typed since. The seat to name is the one waiting, which is
      // the only thing the error's own shape can be read against.
      for (const key of echoes.outstanding()) echoes.refuse(key, message.why);
    });
  });

  /**
   * Stop the turn this seat is running, which the terminal binds to Escape.
   *
   * Fire-and-forget like a send: the turn ending is what says the stop landed,
   * and it arrives as the header's own state going quiet rather than as a
   * reply. The draft is left alone, since the reader may be steering with it.
   */
  function stop(): void {
    try {
      void connection.dispatch({ cancel: { key: slot } });
    } catch (error) {
      // A closed socket has nothing to stop, and the control goes with the
      // turn that would have drawn it - but the click did nothing and that is
      // reported rather than swallowed.
      report('the stop was not sent', error);
    }
  }

  /** Send the draft, and remember the command when the draft was one. */
  function send(): void {
    // Trimmed at the ends like the terminal's own submit: picking a row off the
    // list leaves a trailing space for the next word, and an answer that goes
    // out as `/compact ` is one nothing asked for.
    const text = box.draft.trim();
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
    if (first.startsWith('/')) box.sent = first;
    // What the seat was doing when the words left: a send that starts a turn is
    // taken by it, one sent into a turn already running is not (it is settled by
    // the conversation carrying the words, or by a refusal).
    echoes.post(boxKey(slot), text, running);
    box.draft = '';
  }

  /**
   * The keys the field answers to, which are two surfaces in one slot: while a
   * list is open its keys win, and otherwise Enter sends.
   */
  function onkey(event: KeyboardEvent): void {
    if (list !== null && list.rows.length > 0) {
      if (event.key === 'ArrowDown') {
        event.preventDefault();
        box.marked = (box.marked + 1) % list.rows.length;
        return;
      }
      if (event.key === 'ArrowUp') {
        event.preventDefault();
        box.marked = (box.marked - 1 + list.rows.length) % list.rows.length;
        return;
      }
      if (event.key === 'Escape') {
        event.preventDefault();
        box.closed = box.draft;
        return;
      }
      if (event.key === 'Enter') {
        event.preventDefault();
        pick(box.marked);
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
    box.draft = `${box.draft.slice(0, list.from)}${row.insert} `;
    box.closed = null;
    // The click landed on the row, so the field takes the keyboard back with
    // the words - the reader is typing again rather than having chosen a button.
    field?.focus();
  }

  /** The reader's own typing is what dismisses a notice row. */
  function oninput(): void {
    box.dismissed = notice?.text ?? null;
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

  /**
   * Remember which prompt this reader answered, while the core still lists it.
   *
   * A new answer supersedes the refusal it followed: the reason belonged to the
   * attempt that failed, and left standing it would read as a verdict on this
   * one.
   */
  function remember(toolId: string | null): void {
    box.answered = toolId;
    box.answeredKey = ownKeyOf(ask);
    box.refusal = null;
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
   * A held Slack post carries no tool id and rides its own instead, so two in a
   * row are two prompts: the dock has rows and a mark, and a shared key would
   * carry the one draft's mark onto the next.
   */
  function ownKeyOf(current: ReturnType<typeof pendingAsk>): string | null {
    if (current === null) return null;
    if (current.kind === 'permission') return `permission:${current.request.toolId}`;
    if (current.kind === 'question') {
      return `question:${current.request.toolId}:${String(current.request.index)}`;
    }
    return `slack:${current.request.id}`;
  }

  /**
   * What the prompt is waiting on, which is how an answer is told apart from the
   * next one.
   *
   * A tool id for a permission and a question, and the draft's own id for a held
   * post: it is answered by that id rather than by a tool call, and it is what
   * tells a refusal about this draft from one about the next.
   */
  function askToolId(current: ReturnType<typeof pendingAsk>): string | null {
    if (current === null) return null;
    if (current.kind === 'slack_draft') return current.request.id;
    return current.request.toolId;
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
{:else if dockAsk !== null}
  <div class="comp">
    <!-- Keyed on the prompt, so a batch's next question is a fresh dock: the
         wire's option ids are positional, so a mark or a toggle left over from
         the question before is a valid answer to the one after, and the core
         accepts it. A repaint of the same prompt keeps its key, so a frame
         arriving clears nothing the reader has turned on. -->
    {#key ownKeyOf(dockAsk)}
      <Dock
        ask={dockAsk}
        ownKey={ownKeyOf(dockAsk)}
        {slot}
        {connection}
        depth={seat.pendingDepth}
        notice={box.refusal}
        take={composer.take}
        bind:notes={dockDraft}
        bind:ownOpen={dockOpen}
        land={dockLanded}
        onanswer={remember}
        onabandon={abandon}
        answered={box.answeredKey !== null &&
          box.answeredKey === ownKeyOf(dockAsk) &&
          box.refusal === null}
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
      class:rec={ring === 'rec'}
      class:tr={ring === 'tr'}
      class:done={ring === 'done'}
    >
      {#if composer.take !== null}
        <Dictation take={composer.take} {slot} {connection} />
      {:else if line !== null}
        <div class="notice {line.tone}">{line.text}</div>
      {/if}
      {#if list !== null}
        <Autocomplete offer={list} marked={box.marked} onpick={pick} />
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
            activeDescendant: list === null ? undefined : rowId(list, box.marked),
          }}
          bind:value={box.draft}
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
        {#if running}
          <button class="stop" type="button" title="stop" aria-label="stop" onclick={stop}>
            <Icon name="stop" />
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
