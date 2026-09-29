<script lang="ts">
  import Icon from '../components/Icon.svelte';
  import type { Connection } from '../socket';
  import type { SessionSlot } from '../wire/types';
  import type { Ask, PermissionOption, Take } from './wire';

  let {
    ask,
    slot,
    connection,
    depth = 1,
    notice = null,
    take = null,
    onanswer = () => {},
    onabandon = () => {},
  }: {
    ask: Ask;
    slot: SessionSlot;
    connection: Pick<Connection, 'dispatch'>;
    /** How many prompts wait behind this one, which the queue line states. */
    depth?: number;
    /** Why the core refused an answer to this prompt, when it did. */
    notice?: string | null;
    /** A take still running behind the dock, which the blip names. */
    take?: Take | null;
    onanswer?: (toolId: string | null) => void;
    onabandon?: () => void;
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
     * It reveals the field rather than answering: a row labelled "tell Claude
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
          label: 'Tell Claude something else:',
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
  /** What the reader has said in their own words, which the notes row carries. */
  let notes = $state('');
  /** The field, so marking the own-words row can put the caret in it. */
  let field = $state<HTMLTextAreaElement | null>(null);
  /** The listbox, which owns the keys while the dock has the slot. */
  let listbox = $state<HTMLDivElement | null>(null);

  const markedRow = $derived(rows[marked]);
  const notesOpen = $derived(markedRow !== undefined && markedRow.own);

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

  /** The tool the prompt is waiting on, which an answer is addressed by. */
  const toolId = $derived(
    ask.kind === 'permission' || ask.kind === 'question' ? ask.request.toolId : null,
  );

  /** Clicking a row: an option answers, the own-words row opens the field. */
  function choose(at: number): void {
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
    if (row === undefined || toolId === null) return;
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
        // Nothing chosen and nothing said is not an answer, it is a rejection -
        // the terminal's own rule, kept here so the core is told which it was.
        outcome:
          ids.length === 0 && annotation === null
            ? { outcome: 'cancelled' }
            : { outcome: 'answered', selected_option_ids: ids, annotation },
      },
    });
  }

  function move(step: number): void {
    if (rows.length === 0) return;
    marked = (marked + step + rows.length) % rows.length;
  }

  /**
   * Turn the marked row on or off, which only a multi-select question does.
   *
   * The key the chip names has to do this from wherever the dock's keys are
   * read: the rows are not focusable, so a handler on a row would be a handler
   * nothing reaches.
   */
  function toggle(): void {
    const row = rows[marked];
    if (!multi || row === undefined || row.optionId === null) return;
    toggled = toggled.includes(row.optionId)
      ? toggled.filter((id) => id !== row.optionId)
      : [...toggled, row.optionId];
  }

  /** Whether a key landed in the field the own-words row opened. */
  const inField = (event: KeyboardEvent): boolean =>
    event.target instanceof HTMLElement && event.target.classList.contains('notes');

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
        // Back to the options, which is where the terminal's Escape goes from
        // its own notes editor - and the words typed so far stay in the state.
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
    // A question's rows draw a box per option and its answer carries one of
    // them, so a reject key would name a key that cannot do what it says.
    if (event.key === 'Escape' && !question) {
      event.preventDefault();
      const deny = rows.findIndex((row) => row.tone === 'no');
      marked = deny < 0 ? rows.length - 1 : deny;
      submit();
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
    <div class="queue">
      <Icon name="chev" class="more" />
      {depth - 1} more pending after this
    </div>
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
      <span class="q">Q{ask.request.index + 1} of {ask.request.total}</span>
    </div>
    <div class="desc">{ask.request.question}</div>
  {:else}
    <div class="head">
      <span class="t">
        {ask.request.threadTs === null ? 'Post to Slack' : 'Reply in Slack'}
      </span>
      <span class="q">
        {ask.request.workspace} · {ask.request.conversationLabel}
      </span>
    </div>
    <!-- This client holds no approval dock for a held post, which is the
         session view's Slack surface rather than the composer's. What it can
         do is say what is waiting, rather than explain an empty dock away. -->
    <div class="desc">
      {ask.request.tool} is waiting to send: approve it from the terminal, and the words land in your
      draft either way.
    </div>
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
            // What it does with them is what the keys line promises, which is
            // the listbox's own rule: Space toggles, Enter submits. Arrows and
            // Escape still bubble, because those belong to the listbox.
            event.stopPropagation();
            marked = at;
            if (key === ' ') {
              toggle();
              return;
            }
            submit();
          }}
        >
          {#if row.icon !== null}
            <Icon name={row.icon} class={row.tone} />
          {:else}
            <span class="box2" class:on={row.optionId !== null && toggled.includes(row.optionId)}>
              {#if row.optionId !== null && toggled.includes(row.optionId)}
                <Icon name="check" />
              {/if}
            </span>
          {/if}
          <span class="lbl">{row.label}</span>
          {#if row.detail !== null}
            <span class="d">{row.detail}</span>
          {/if}
        </div>
      {/each}
    </div>

    {#if markedRow?.preview != null}
      <div class="preview">{markedRow.preview}</div>
    {/if}

    {#if notesOpen}
      <!-- The reader's own words, which the answer carries as its annotation
           rather than as an option id. -->
      <textarea
        class="notes"
        bind:this={field}
        bind:value={notes}
        rows="1"
        placeholder="answer with your own words"
        onkeydown={onkey}></textarea>
    {/if}

    <div class="keys">
      <span><kbd>↑</kbd><kbd>↓</kbd> {question ? 'move' : 'select'}</span>
      {#if multi}
        <span><kbd>space</kbd> toggle</span>
      {/if}
      <span><kbd>Enter</kbd> {question ? 'submit' : 'confirm'}</span>
      {#if take !== null}
        <span><kbd>Esc</kbd> cancel the take</span>
      {:else if !question}
        <span><kbd>Esc</kbd> reject</span>
      {/if}
    </div>

    {#if take !== null}
      <div class="blip">
        <span class="dot" class:tr={take.phase === 'transcribing'}></span>
        dictating · the words land in your draft either way
      </div>
    {/if}
  {/if}
</div>
