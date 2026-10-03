<script lang="ts">
  import Prose from '../chat/Prose.svelte';
  import Icon from '../components/Icon.svelte';
  import Field from './Field.svelte';
  import type { Connection } from '../socket';
  import type { SessionSlot } from '../wire/types';
  import type { Ask, PermissionOption, Take } from './wire';

  let {
    ask,
    ownKey = null,
    slot,
    connection,
    depth = 1,
    notice = null,
    take = null,
    notes = $bindable(''),
    ownOpen = $bindable(false),
    land = null,
    onanswer = () => {},
    onabandon = () => {},
    answered = false,
  }: {
    ask: Ask;
    /**
     * The prompt's own identity, which the composer computes (`ownKeyOf`).
     *
     * The record is replaced on every frame and on the session poll, so `ask`
     * is a fresh object while the prompt is the same one - the dock keys its
     * keyboard-taking on this rather than on that object, so a re-render is
     * not a new prompt.
     */
    ownKey?: string | null;
    slot: SessionSlot;
    connection: Pick<Connection, 'dispatch'>;
    /** How many prompts wait behind this one, which the queue line states. */
    depth?: number;
    /** Why the core refused an answer to this prompt, when it did. */
    notice?: string | null;
    /** A take still running behind the dock, which the blip names. */
    take?: Take | null;
    /**
     * What the reader has said in their own words.
     *
     * Held by the composer rather than here, because a landed take has to reach
     * whichever box the table names and this one has no other way in.
     */
    notes?: string;
    /** Whether that box is open, which is what makes this dock a destination. */
    ownOpen?: boolean;
    /** A take's words that landed in this box, which come with the keyboard. */
    land?: string | null;
    onanswer?: (toolId: string | null) => void;
    onabandon?: () => void;
    /**
     * Whether this is the prompt the reader has answered and the core has not
     * taken yet.
     *
     * **The dock stands where the terminal pops it**, and the mark says so:
     * the pick is drawn at once and everything else on the dock stands down,
     * so a second Enter is not read as a second answer to a prompt the reader
     * has already answered. A refusal brings it back live, with the reason.
     */
    answered?: boolean;
  } = $props();

  /** Which sprite carries an option's meaning, and the colour the sheet gives it. */
  const ICONS: Record<PermissionOption['kind'], string> = {
    allow: 'check',
    deny: 'x',
    edit: 'edit',
    notes: 'dots',
  };
  const TONES: Record<PermissionOption['kind'], string> = {
    allow: 'ok',
    deny: 'no',
    edit: 'ed',
    notes: '',
  };

  /**
   * One row as the dock draws it, whichever prompt it came from.
   *
   * A question's rows and a permission's are the same row with a different
   * marker - a checkbox where a question draws one, the option's own icon
   * where a permission does - so they are built once here rather than drawn
   * twice below.
   */
  interface Row {
    key: string;
    /** What the row's own marker is: the meaning's icon, or the question's box. */
    icon: string | null;
    tone: string;
    label: string;
    /** What choosing it means, which a question's option carries. */
    detail: string | null;
    /** What it would do, which the marked row shows while the reader decides. */
    preview: string | null;
    /** The option this row answers with, or `null` for the reader's own words. */
    optionId: string | null;
    /**
     * Whether this row is where the reader says something in their own words.
     *
     * It reveals the field rather than answering: a row labelled "tell the agent
     * something else" that answers with nothing said is a key that cannot do
     * what it says, one level down.
     */
    own: boolean;
  }

  const rows = $derived<Row[]>(rowsOf(ask));
  const question = $derived(ask.kind === 'question');
  const multi = $derived(ask.kind === 'question' && ask.request.multiSelect);

  function rowsOf(prompt: Ask): Row[] {
    if (prompt.kind === 'permission') {
      return prompt.request.options.map((option) => ({
        key: option.optionId,
        icon: ICONS[option.kind],
        tone: TONES[option.kind],
        label: option.name,
        detail: null,
        preview: null,
        optionId: option.optionId,
        own: option.kind === 'notes',
      }));
    }
    if (prompt.kind === 'slack_draft') {
      // A held post has two answers and no more: the draft goes out, or it does
      // not. The rows are the dock's own because the wire carries the draft
      // rather than a set of options to choose from.
      return [
        {
          key: 'send',
          icon: 'check',
          tone: 'ok',
          label: 'Send it',
          detail: null,
          preview: null,
          optionId: null,
          own: false,
        },
        {
          key: 'drop',
          icon: 'x',
          tone: 'no',
          label: "Don't send",
          detail: null,
          preview: null,
          optionId: null,
          own: false,
        },
      ];
    }
    if (prompt.kind === 'question') {
      return [
        ...prompt.request.options.map((option) => ({
          key: option.optionId,
          icon: null,
          tone: 'ok',
          label: option.label,
          detail: option.description,
          preview: option.preview,
          optionId: option.optionId,
          own: false,
        })),
        // The escape hatch the mockup draws: words rather than a choice, which
        // is an answer the core accepts with nothing selected.
        {
          key: 'own',
          icon: null,
          tone: 'ok',
          label: 'Tell the agent something else:',
          detail: null,
          preview: null,
          optionId: null,
          own: true,
        },
      ];
    }
    return [];
  }

  /** Which row a key would take, which is the first until one moves it. */
  let marked = $state(0);
  /** The options a multi-select question has toggled, in the order they were. */
  let toggled = $state<string[]>([]);
  /** The field, so marking the own-words row can put the caret in it. */
  let field = $state<HTMLElement | null>(null);
  /** The listbox, which owns the keys while the dock has the slot. */
  let listbox = $state<HTMLDivElement | null>(null);

  const markedRow = $derived(rows[marked]);
  const notesOpen = $derived(markedRow !== undefined && markedRow.own);

  // The dock takes the keyboard the moment it arrives, and again for every
  // question of a batch: its rows are the only thing to answer, a prompt nobody
  // has clicked answers no keys at all, and the listbox element survives the
  // swap from one question to the next - so an effect keyed on the element
  // alone never re-ran and the keys were dead at the start of every question
  // after the first.
  //
  // **Keyed on the prompt's own identity, not on the object a record mints for
  // it.** The record is replaced on every frame and on the session poll, and a
  // re-render of the SAME prompt used to hand the dock the keyboard again: a
  // reader typing in the notes row lost the caret to the option list
  // mid-sentence, and the rest of their typing went to the listbox. While that
  // box is open the caret is the reader's, so this leaves it where they put it.
  $effect(() => {
    void ownKey;
    if (notesOpen) return;
    if (listbox !== null) listbox.focus({ preventScroll: true });
  });

  // The composer decides where a take's words land, and this dock is only a
  // destination while its box is open - so it has to say whether it is.
  //
  // The cleanup is load-bearing: a seat can fail with a prompt still waiting,
  // which puts the blocker in the slot and takes this dock off screen while the
  // prompt stays, and a `true` left standing here would route a take's words to
  // a box that is no longer there.
  $effect(() => {
    ownOpen = notesOpen;
    return () => {
      ownOpen = false;
    };
  });

  /**
   * Focus follows the marked row.
   *
   * The box morphed into this, so the keyboard is already where the reader's
   * hands are - and the dock's keys live on the listbox, which is why a dock
   * that nothing focuses is a dock whose arrows do nothing. Marking the
   * own-words row moves the caret into the field for the same reason.
   */
  $effect(() => {
    const wanted = notesOpen ? field : listbox;
    if (wanted !== null) wanted.focus();
  });

  // A take's words come with the keyboard, and this is the box they landed in.
  $effect(() => {
    if (land === null) return;
    if (field !== null) field.focus();
  });

  /** The tool the prompt is waiting on, which an answer is addressed by. */
  const toolId = $derived(
    ask.kind === 'permission' || ask.kind === 'question' ? ask.request.toolId : null,
  );

  /** Clicking a row: an option answers, the own-words row opens the field. */
  function choose(at: number): void {
    if (answered) return;
    const row = rows[at];
    if (row === undefined) return;
    marked = at;
    if (row.own) {
      // A click here reveals the field; submitting what has not been written
      // is not an answer, and the core reads it as a cancel.
      if (field !== null) field.focus();
      return;
    }
    if (multi && row.optionId !== null) {
      toggled = toggled.includes(row.optionId)
        ? toggled.filter((id) => id !== row.optionId)
        : [...toggled, row.optionId];
      return;
    }
    submit();
  }

  /**
   * Answer with what the reader chose.
   *
   * The outcome is built from the options the core sent rather than from
   * anything this component invented: an option's own `action` is what decides
   * what happens, and a client naming its own could allow what the prompt never
   * offered.
   */
  function submit(): void {
    const row = rows[marked];
    if (row === undefined) return;

    if (ask.kind === 'slack_draft') {
      // A draft is answered by its own id rather than by a tool call, which is
      // the command the terminal sends for the same two rows. Both are read
      // before the answer is remembered: remembering it is what takes this dock
      // off screen, and the prop is gone by the next read.
      const id = ask.request.id;
      const approved = row.key === 'send';
      onanswer(id);
      void connection.dispatch({ respond_slack_post: { key: slot, id, approved } });
      return;
    }

    if (toolId === null) return;
    const words = notes.trim() === '' ? null : notes;

    if (ask.kind === 'permission') {
      if (row.optionId === null) return;
      const option = ask.request.options.find((held) => held.optionId === row.optionId);
      if (option === undefined) return;
      // The own-words row asks for words: submitting it with none says nothing
      // AND denies on the reader's behalf, so it waits for them to write.
      if (option.kind === 'notes' && words === null) return;
      onanswer(toolId);
      void connection.dispatch({
        respond_permission: {
          key: slot,
          tool_id: toolId,
          outcome: {
            outcome: 'selected',
            option_id: option.optionId,
            action: option.action,
            // A permission's own-words row is a deny carrying what was said;
            // without it the label would promise a message nobody sends.
            ...(option.kind === 'notes' && words !== null ? { notes_text: words } : {}),
          },
        },
      });
      return;
    }

    if (ask.kind !== 'question') return;
    const empty = words === null && (!multi || toggled.length === 0) && row.optionId === null;
    if (empty) {
      // The own-words row asks for words, and Enter on it with nothing said has
      // no answer to send - sending one rejects the question on the reader's
      // behalf, which is the defect the permission's own-words guard closes one
      // branch up. It opens the field instead, the same as clicking it does.
      if (field !== null) field.focus();
      return;
    }
    // A multi-select question carries every row that is on, and the marked one
    // when nothing is: submitting a set the reader never saw is worse than
    // submitting the row their key is on.
    const ids = multi && toggled.length > 0 ? toggled : row.optionId === null ? [] : [row.optionId];
    const annotation = words === null ? null : { preview: null, notes: words };
    onanswer(toolId);
    void connection.dispatch({
      respond_question: {
        key: slot,
        tool_id: toolId,
        // Every answer from here carries something - a row, a set, or words -
        // because the empty case above is not an answer. A question has no
        // reject in this dock: the terminal's Escape does that, and Escape
        // here belongs to a take and then to a permission.
        outcome: { outcome: 'answered', selected_option_ids: ids, annotation },
      },
    });
  }

  /**
   * Answer with the prompt's own way out.
   *
   * A question refuses with an outcome of its own rather than a row, which is
   * the shape the core offers and the one the terminal sends - a question whose
   * options are all wrong has no row to say so with, and an empty `answered`
   * would pick nothing on the reader's behalf.
   */
  function reject(): void {
    if (ask.kind === 'question') {
      onanswer(ask.request.toolId);
      void connection.dispatch({
        respond_question: {
          key: slot,
          tool_id: ask.request.toolId,
          outcome: { outcome: 'cancelled' },
        },
      });
      return;
    }
    // Every other prompt refuses with a row of its own: choosing that row keeps
    // the action the core sent as the action taken, rather than one this client
    // named.
    const deny = rows.findIndex((row) => row.tone === 'no');
    marked = deny < 0 ? rows.length - 1 : deny;
    submit();
  }

  function move(step: number): void {
    if (rows.length === 0) return;
    marked = (marked + step + rows.length) % rows.length;
  }

  /**
   * Turn the marked row on or off, which only a multi-select question does.
   *
   * The key the chip names has to do this from wherever the dock's keys are
   * read, and there are two places: the listbox, which holds the keyboard by
   * default, and a row a click has focused. A row is focusable on click but not
   * by Tab - `tabindex="-1"` - which is why a handler on one is reachable at
   * all rather than dead code to be tidied away.
   */
  function toggle(): void {
    const row = rows[marked];
    if (!multi || row === undefined || row.optionId === null) return;
    toggled = toggled.includes(row.optionId)
      ? toggled.filter((id) => id !== row.optionId)
      : [...toggled, row.optionId];
  }

  /**
   * Whether a key landed in the field the own-words row opened.
   *
   * By the editor it names rather than by its class: the class is the sheet's
   * styling hook, and a restyle that renamed it would silently reroute keys.
   */
  const inField = (event: KeyboardEvent): boolean =>
    event.target instanceof HTMLElement && event.target.closest('[data-editor="dock"]') !== null;

  /** The keys a dock answers to, which are the ones that can do what they say. */
  function onkey(event: KeyboardEvent): void {
    if (event.key === 'Escape' && take !== null) {
      // A live take owns the first Escape: it is abandoned and the dock stands,
      // which is the terminal's own rule for the same slot.
      event.preventDefault();
      onabandon();
      return;
    }
    if (inField(event)) {
      // The reader is writing. Enter submits what they wrote, and the arrows
      // move the mark - which is the way back OUT of the field, because the
      // mark leaving the own-words row is what hands the keyboard back to the
      // list. Everything else stays the field's own, so a caret still moves.
      if (event.key === 'Enter' && !event.shiftKey) {
        event.preventDefault();
        submit();
        return;
      }
      if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
        event.preventDefault();
        move(event.key === 'ArrowDown' ? 1 : -1);
        return;
      }
      if (event.key === 'Escape') {
        // A question's way out from here is the reject, which is the terminal's
        // own: its notes box cancels the prompt. The words typed so far go with
        // it, because the prompt does.
        if (question) {
          event.preventDefault();
          reject();
          return;
        }
        // A permission steps back to its options instead, so the words typed so
        // far stay in the state - a divergence from the terminal, which cancels
        // from its notes box for this kind too.
        event.preventDefault();
        move(-1);
      }
      return;
    }
    if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
      event.preventDefault();
      move(event.key === 'ArrowDown' ? 1 : -1);
      // A row can be holding the keyboard when the mark moves - a click leaves
      // focus on it - and the mark is what Space and Enter act on, so the
      // keyboard goes where the mark is. `focus follows the mark` is the rule
      // the effect above enforces for the own-words row; this is the other half
      // of it, for a mark that moved off a row that still had focus.
      listbox?.focus();
      return;
    }
    if (event.key === ' ') {
      event.preventDefault();
      toggle();
      return;
    }
    if (event.key === 'Enter') {
      event.preventDefault();
      submit();
      return;
    }
    if (event.key === 'Escape') {
      event.preventDefault();
      reject();
    }
  }

  /** The id the listbox points at, which is how a reader hears which row is marked. */
  function rowId(at: number): string {
    const row = rows[at];
    return `dock-${toolId ?? 'ask'}-${row === undefined ? at : row.key}`;
  }
</script>

<div class="dock">
  {#if depth > 1}
    <!-- The words say it; a marker in front of them was one more thing to
         decode on a row that is already a count. -->
    <div class="queue">{depth - 1} more pending after this</div>
  {/if}
  {#if notice !== null}
    <div class="notice bad">{notice}</div>
  {/if}

  {#if ask.kind === 'permission'}
    <div class="head">
      <span class="t mono">{ask.request.title}</span>
      {#if ask.request.subject !== ''}
        <span class="q">{ask.request.subject}</span>
      {/if}
    </div>
    {#if ask.request.reason !== null}
      <div class="reason">{ask.request.reason}</div>
    {/if}
    {#if ask.request.description !== null}
      <div class="desc">{ask.request.description}</div>
    {/if}
  {:else if ask.kind === 'question'}
    <div class="head">
      <span class="qm"><Icon name="question" /></span>
      <span class="t">{ask.request.header}</span>
      {#if ask.request.total > 1}
        <span class="q">Q{ask.request.index + 1} of {ask.request.total}</span>
      {/if}
    </div>
    <div class="desc">{ask.request.question}</div>
  {:else}
    <div class="head">
      <span class="t">
        {ask.request.threadTs === null ? 'Post to Slack' : 'Reply in Slack'}
      </span>
      <span class="q">
        {ask.request.workspace} · {ask.request.conversationLabel}{#if ask.request.threadTs !== null}
          · thread {ask.request.threadTs}{/if}
      </span>
    </div>
    <!-- The body verbatim: this is what would go out, so the reader approves
         the text itself rather than a summary of it. -->
    <!-- The draft reads as an option's preview does, through the same
         markdown renderer and the same panel: Slack text IS markdown, the
         question kinds are already in hand, and one kind of block for both
         keeps the dock one system rather than two. -->
    <div class="preview"><Prose text={ask.request.text} /></div>
  {/if}

  {#if rows.length > 0}
    <div
      class="opts"
      role="listbox"
      aria-label="the prompt's options"
      bind:this={listbox}
      tabindex="-1"
      aria-activedescendant={rowId(marked)}
      onkeydown={onkey}
    >
      {#each rows as row, at (row.key)}
        <div
          class="opt"
          class:sel={at === marked}
          role="option"
          id={rowId(at)}
          aria-selected={at === marked}
          tabindex="-1"
          onclick={() => choose(at)}
          onkeydown={(event) => {
            const key = event.key;
            if (key !== 'Enter' && key !== ' ') return;
            event.preventDefault();
            // A row keeps the keys it answers for itself: it is focusable (a
            // browser focuses it on click), and without stopping the event the
            // same key also reaches the listbox's branch behind it and either
            // undoes the toggle or submits what this just chose.
            //
            // What it does with them is NOT restated here - it reaches the same
            // handler the listbox uses, which is the only place the key map
            // lives. Two statements of it disagree the moment one is edited,
            // and this one already did. Arrows and Escape still bubble, because
            // those belong to the listbox and are not this row's to take.
            event.stopPropagation();
            marked = at;
            onkey(event);
          }}
        >
          {#if row.icon !== null}
            <Icon name={row.icon} class={row.tone} />
          {:else if multi}
            <!-- The escape row's box reads checked once there are words in it:
                 display only, because the typed words ride the answer's
                 annotation rather than the selection set - the terminal's own
                 rule, where the box confirms the typed content will go with
                 the answer. -->
            <span
              class="box2"
              class:on={row.own
                ? notes.trim() !== ''
                : row.optionId !== null && toggled.includes(row.optionId)}
            >
              {#if row.own ? notes.trim() !== '' : row.optionId !== null && toggled.includes(row.optionId)}
                <Icon name="check" />
              {/if}
            </span>
          {:else if row.own}
            <!-- The escape row keeps the slot its options mark in - an empty
                 one - so its words start in the same column theirs do. -->
            <span class="slot"></span>
          {:else}
            <!-- A single-answer question marks its rows in the same slot the
                 set draws its boxes in - a circle that fills on the row being
                 taken - so its options are not bare against every other kind
                 of row on the page. -->
            <span class="radio" class:on={at === marked}></span>
          {/if}
          <span class="tx">
            {#if row.own && notesOpen}
              <!-- **The escape hatch IS the box.** Moving onto the row puts the
                   caret in it, so the answer is typed or dictated where the row
                   already says it will be, rather than into a second box
                   opening under the list. The keys below the listbox are the
                   row's own while the caret is in it. -->
              <Field
                editor="dock"
                class="notes"
                bind:value={notes}
                rows={1}
                placeholder={row.label}
                onkeydown={(event: KeyboardEvent) => {
                  event.stopPropagation();
                  onkey(event);
                }}
                field={(el: HTMLElement | null) => {
                  field = el;
                }}
              />
            {:else}
              <span class="lbl">{row.label}</span>
              {#if row.detail !== null}
                <span class="why">{row.detail}</span>
              {/if}
            {/if}
          </span>
        </div>
      {/each}
    </div>

    {#if markedRow?.preview != null}
      <!-- The marked option's own prose, carried as markdown on the wire and
           rendered as markdown rather than shown as its source - the same
           renderer the conversation's prose uses, which escapes what it is
           handed. -->
      <div class="preview"><Prose text={markedRow.preview} /></div>
    {/if}

    {#if answered}
      <!-- The same mark the reader's own words carry while they are on their
           way, because it is the same wait: a pick that has left the reader and
           has not been taken yet. -->
      <div class="keys">
        <span class="answering"
          ><span class="ring"></span>sending · it holds until the core takes it</span
        >
      </div>
    {:else}
      <div class="keys">
        <span><kbd>↑</kbd><kbd>↓</kbd> {question ? 'move' : 'select'}</span>
        {#if multi}
          <span><kbd>space</kbd> toggle</span>
        {/if}
        <span><kbd>Enter</kbd> {question ? 'submit' : 'confirm'}</span>
        {#if take !== null}
          <span><kbd>Esc</kbd> cancel the take</span>
        {:else}
          <span><kbd>Esc</kbd> reject</span>
        {/if}
      </div>
    {/if}

    {#if take !== null}
      <div class="blip">
        <span class="dot" class:tr={take.phase === 'transcribing'}></span>
        dictating · the words land in your draft either way
      </div>
    {/if}
  {/if}
</div>
