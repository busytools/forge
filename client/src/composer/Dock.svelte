<script lang="ts">
  import Prose from '../chat/Prose.svelte';
  import Icon from '../components/Icon.svelte';
  import Field from './Field.svelte';
  import TakeCard from './TakeCard.svelte';
  import type { Command } from '../protocol';
  import { report, type Connection } from '../socket';
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
    wire = null,
    notes = $bindable(''),
    ownOpen = $bindable(false),
    land = null,
    onanswer = () => {},
    onabandon = () => {},
    onmic = () => {},
    dictation = false,
    takeline = null,
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
    /** A take still running behind the dock, which the row it lands in draws. */
    take?: Take | null;
    /**
     * That take's wire side, when this page owns the capture - the card the
     * custom row draws, on the same terms the composer's card draws it.
     */
    wire?: {
      frames: number;
      bytes: number;
      rate: number | null;
      dbfs: number[];
      elapsedMs: number;
    } | null;
    /**
     * The mic on the custom row: this surface's own door to a take.
     *
     * A divergence from the composer's "the mic is the door, not the trigger",
     * and the reason is rule 22's touch door: the terminal's push-to-talk key
     * does not exist on a phone, so a row that could only SHOW a take would
     * leave a finger no way to dictate an answer at all.
     */
    onmic?: () => void;
    /**
     * Whether this install can dictate at all, which gates the mic.
     *
     * The composer's own rule, kept here: a control this install cannot honour
     * is worse than none, and a mic that starts a take the core refuses draws
     * nothing a reader can act on.
     */
    dictation?: boolean;
    /**
     * What became of a take this page started: a failure the server never saw,
     * or a refusal it sent back.
     *
     * Drawn here because the box's own notice row is not on screen while a
     * prompt holds the slot - the take's line would be invisible exactly when
     * the reader pressed the mic that produced it.
     */
    takeline?: { tone: string; text: string } | null;
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

  /** The order a permission's actions are drawn in: deny, edit, then trusting. */
  const RANK: Record<PermissionOption['kind'], number> = { deny: 0, edit: 1, allow: 2, notes: 3 };

  /** One of a question's options, as its row draws it. */
  interface Row {
    key: string;
    label: string;
    /** What choosing it means, which the wire carries per option. */
    detail: string | null;
    /** What it would do, which the marked row shows while the reader decides. */
    preview: string | null;
    /** The option this row answers with. */
    optionId: string;
    /** The row that is a door to the words, not an answer of its own. */
    custom: boolean;
  }

  const rows = $derived<Row[]>(rowsOf(ask));
  const question = $derived(ask.kind === 'question');
  const multi = $derived(ask.kind === 'question' && ask.request.multiSelect);

  /**
   * A permission's actions, which are buttons rather than rows.
   *
   * A decision about a command is not a menu: the actions sit together, the
   * trusting one primary, and the reason field is the one row that takes words.
   * The wire's own options decide which actions there are - a client naming its
   * own could allow what the prompt never offered.
   */
  const actions = $derived(
    ask.kind === 'permission'
      ? [...ask.request.options]
          .filter((option) => option.kind !== 'notes')
          // Deny first and the trusting actions last, so the primary sits at
          // the row's end the way the mock draws it.
          .sort((a, b) => (RANK[a.kind] ?? 9) - (RANK[b.kind] ?? 9))
      : [],
  );
  /** The permission's own-words option, when the wire offers one. */
  const reason = $derived(
    ask.kind === 'permission'
      ? (ask.request.options.find((option) => option.kind === 'notes') ?? null)
      : null,
  );
  /**
   * The ONE action drawn primary: the last trusting one, which is the mock's.
   *
   * Not every allow: the wire's common prompt offers two of them, and marking
   * both is no hierarchy at all.
   */
  const primary = $derived(
    [...actions].reverse().find((option) => option.kind === 'allow')?.optionId ?? null,
  );

  function rowsOf(prompt: Ask): Row[] {
    if (prompt.kind === 'permission') {
      // A permission draws no rows: its actions are buttons and its words are
      // the reason field, so there is nothing here for the listbox to hold.
      return [];
    }
    if (prompt.kind === 'slack_draft') {
      // A held post draws no rows either: the draft goes out or it does not,
      // and those two verbs are buttons under the body they would send.
      return [];
    }
    if (prompt.kind === 'question') {
      return [
        ...prompt.request.options.map((option) => ({
          key: option.optionId,
          label: option.label,
          detail: option.description,
          preview: option.preview,
          optionId: option.optionId,
          custom: false,
        })),
        // **The custom answer as an option of its own**, where the reader
        // looks for every other choice: picking it opens the words row rather
        // than answering, and what is written there goes as the answer alone.
        {
          key: 'custom',
          label: 'Tell the agent something else',
          detail: null,
          preview: null,
          optionId: 'custom',
          custom: true,
        },
      ];
    }
    return [];
  }

  /** Which row a key would take, which is the first until one moves it. */
  let marked = $state(0);
  /** The options a multi-select question has toggled, in the order they were. */
  let toggled = $state<string[]>([]);

  /** What the submit key says, with the count riding it so a set is never sent unseen. */
  const submitLabel = $derived(
    multi && toggled.length > 0 ? `submit ${String(toggled.length)}` : 'submit',
  );
  /** The field the custom row draws, which is also where a take's words land. */
  let field = $state<HTMLElement | null>(null);
  /** The listbox, which owns the keys while the dock has the slot. */
  let listbox = $state<HTMLDivElement | null>(null);
  /** The dock itself, which holds the keyboard for a prompt that draws no row. */
  let root = $state<HTMLDivElement | null>(null);
  /** Whether the reader has been in the words row, which outlives the row. */
  let visited = $state(false);
  /**
   * The row this dock answered with, or `null` for an answer from the words.
   *
   * The wait is drawn where the answer came from, so it has to be remembered
   * here: `answered` says an answer is on its way, not which one it was.
   */
  let answeredRow = $state<number | null>(null);

  const markedRow = $derived(rows[marked]);

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
  // reader typing in the field lost the caret to the option list mid-sentence,
  // and the rest of their typing went to the listbox. While the caret is in the
  // field it is the reader's, so this leaves it where they put it.
  $effect(() => {
    void ownKey;
    if (inFieldNow()) return;
    // The options where there are any, and the dock itself where there are not
    // (a permission, a held post) - NOT the words row: focusing that would make
    // this dock the destination for a take's words, and a landing sent to a row
    // that unmounts with the prompt loses them.
    const wanted = listbox ?? root;
    if (wanted !== null) wanted.focus({ preventScroll: true });
  });

  /**
   * The caret across a take: into the dock while the card draws, and back into
   * the words row when it returns.
   *
   * A take replaces the field with its card, so the caret has to live somewhere
   * the dock's own keys still reach - on the page body the typing goes nowhere
   * and the dock's Escape copy is unreachable. And a reader who was writing in
   * the row is put back in it, rather than left in the option list with their
   * words a key away from being answered with the marked row.
   */
  $effect(() => {
    if (!visited) return;
    const el = field ?? root;
    if (el !== null) el.focus({ preventScroll: true });
  });

  /** Whether the caret is in the custom row's field right now. */
  function inFieldNow(): boolean {
    return field !== null && field.contains(document.activeElement);
  }

  // The composer decides where a take's words land, and this dock is a
  // destination once the reader has been in its words row - the analogy of the
  // row being OPENED, which it used to be revealed by and now always is.
  //
  // So it is sticky rather than tied to the caret: a reader who wrote a line and
  // then clicked away is still writing there, and a landing sent to the
  // composer's draft instead would be a box they had left. It is NOT set by the
  // row merely being drawn, because a landing sent to a row nobody has used
  // goes down with the dock when the prompt resolves.
  //
  // The cleanup is load-bearing twice over: a seat can fail with a prompt still
  // waiting, which puts the blocker in the slot and takes this dock off screen
  // while the prompt stays, and a `true` left standing here would route a
  // take's words to a box that is no longer there.
  $effect(() => {
    const el = field;
    if (el === null) return;
    const on = (): void => {
      visited = true;
    };
    el.addEventListener('focus', on);
    if (el === document.activeElement) visited = true;
    return () => {
      el.removeEventListener('focus', on);
    };
  });

  // The destination itself, which outlives the field: a take drawing in the row
  // replaces the field while it runs, and a reader who was writing there is
  // still writing there when it comes back.
  $effect(() => {
    ownOpen = visited;
    return () => {
      ownOpen = false;
    };
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

  /** Clicking a row: a single-answer question takes it, a set toggles it. */
  function choose(at: number): void {
    if (answered) return;
    const row = rows[at];
    if (row === undefined) return;
    marked = at;
    if (row.custom) {
      // The custom row is a door, not an answer: it opens the words row and
      // takes the caret, and the answer goes when there is something to send.
      visited = true;
      field?.focus();
      return;
    }
    if (multi) {
      toggled = toggled.includes(row.optionId)
        ? toggled.filter((id) => id !== row.optionId)
        : [...toggled, row.optionId];
      return;
    }
    submit(false);
  }

  /**
   * Send one answer, and do not let a closed socket take the click with it.
   *
   * `dispatch` throws synchronously when the socket is not open, which lands
   * in the click handler rather than in anything that draws: the loss is
   * reported, and the dock the reader has already answered keeps its
   * stand-down.
   */
  function answer(command: Command): void {
    try {
      void connection.dispatch(command);
    } catch (error) {
      report('the answer was not sent', error);
    }
  }

  /**
   * Answer with what the reader chose.
   *
   * The outcome is built from the options the core sent rather than from
   * anything this component invented: an option's own `action` is what decides
   * what happens, and a client naming its own could allow what the prompt never
   * offered.
   *
   * `fromField` is Enter pressed in the words row: the answer is then what was
   * written, with no row taken - which is the one answer a set of options
   * cannot express, and the reason that row is always drawn.
   */
  function submit(fromField: boolean): void {
    // **An answered prompt takes no second answer.** The stand-down is the
    // reader's mark; a key that reaches here anyway would send the same answer
    // twice under the same tool id.
    if (answered) return;
    const row = rows[marked];
    if (row === undefined) return;
    if (row.custom && !fromField) {
      // Enter on the custom row opens the words, the way choosing it does.
      visited = true;
      field?.focus();
      return;
    }

    if (toolId === null) return;
    if (ask.kind !== 'question') return;
    const words = notes.trim() === '' ? null : notes;
    if (fromField && words === null && (!multi || toggled.length === 0)) {
      // Nothing written and nothing toggled: there is no answer to send, and
      // sending one would reject the question on the reader's behalf.
      return;
    }
    // A multi-select question carries every row that is on, and the marked one
    // when nothing is: submitting a set the reader never saw is worse than
    // submitting the row their key is on. An answer from the field carries no
    // row at all unless the reader turned some on.
    const ids = multi && toggled.length > 0 ? toggled : fromField ? [] : [row.optionId];
    const annotation = words === null ? null : { preview: null, notes: words };
    // Which row was answered, so the wait draws there - and nothing for an
    // answer from the words, which has no row to ride.
    answeredRow = fromField ? null : marked;
    onanswer(toolId);
    answer({
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
    // Same door as `submit`: a prompt already answered takes no refusal.
    if (answered) return;
    // The wait a refused answer left names a row this refusal is not about, so
    // the mark it would draw belongs to the answer that is gone.
    answeredRow = null;
    if (ask.kind === 'question') {
      onanswer(ask.request.toolId);
      answer({
        respond_question: {
          key: slot,
          tool_id: ask.request.toolId,
          outcome: { outcome: 'cancelled' },
        },
      });
      return;
    }
    if (ask.kind === 'permission') {
      // A permission refuses with its own deny action, which is the option the
      // core sent rather than one this client named.
      if (denyOption !== null) decide(denyWith);
      return;
    }
    // A held post refuses with its own "don't send", which is the answer the
    // core reads as the draft going nowhere.
    post(false);
  }

  /** A permission's own deny, which its words ride when the field has any. */
  const denyOption = $derived(actions.find((option) => option.kind === 'deny') ?? null);

  /**
   * The option a deny answers with: the wire's own-words option when the field
   * has words, and the plain deny when it does not.
   *
   * A permission's own-words option IS a deny carrying what was said, so the
   * words ride it rather than the bare deny - without them the label would
   * promise a message nobody sends.
   */
  const denyWith = $derived(notes.trim() !== '' && reason !== null ? reason : denyOption);

  /**
   * A permission's action, clicked or keyed: the option IS the answer.
   *
   * The outcome is built from the option the core sent - its own id and its own
   * action - so a client can never allow what the prompt did not offer.
   */
  function decide(option: PermissionOption | null): void {
    if (option === null || answered || toolId === null) return;
    const words = notes.trim() === '' ? null : notes;
    // The own-words option asks for words: sending it with none says nothing AND
    // denies on the reader's behalf, so it waits for them to write.
    if (option.kind === 'notes' && words === null) return;
    onanswer(toolId);
    answer({
      respond_permission: {
        key: slot,
        tool_id: toolId,
        outcome: {
          outcome: 'selected',
          option_id: option.optionId,
          action: option.action,
          ...(option.kind === 'notes' && words !== null ? { notes_text: words } : {}),
        },
      },
    });
  }

  /** A held post's two verbs, which are buttons rather than rows. */
  function post(approved: boolean): void {
    if (answered || ask.kind !== 'slack_draft') return;
    const id = ask.request.id;
    onanswer(id);
    answer({ respond_slack_post: { key: slot, id, approved } });
  }

  function move(step: number): void {
    // An answered prompt is standing down: its rows are a record of what was
    // answered, not a list to move through.
    if (rows.length === 0 || answered) return;
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
    if (!multi || answered || row === undefined || row.optionId === null) return;
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
    // **A button answers its own keys.** The row model's Enter and Space are
    // for the list; on a focused button they would preventDefault the
    // activation away, which leaves a permission with NO keyboard path to its
    // actions - and on a question the mic is a tab stop, so Enter there would
    // answer the marked row instead.
    if (event.target instanceof HTMLButtonElement && (event.key === 'Enter' || event.key === ' ')) {
      return;
    }
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
        submit(true);
        return;
      }
      if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
        // The arrows move the mark AND hand the keyboard back to the options:
        // the words row is a row like any other, and a reader who arrows out of
        // it is back among the choices rather than stuck writing.
        event.preventDefault();
        move(event.key === 'ArrowDown' ? 1 : -1);
        listbox?.focus();
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
        // A permission steps back out of the field instead, so the words typed
        // so far stay in the state - a divergence from the terminal, which
        // cancels from its notes box for this kind too.
        event.preventDefault();
        if (rows.length > 0) move(-1);
        if (listbox !== null) listbox.focus();
        else root?.focus();
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
    if (/^[1-9]$/.test(event.key)) {
      // A row's number answers it: a single-answer question submits that row,
      // and a multi-select toggles it - which is the same thing the row does
      // when it is clicked.
      const at = Number(event.key) - 1;
      if (at < rows.length) {
        event.preventDefault();
        choose(at);
      }
      return;
    }
    if (event.key === 'Enter') {
      event.preventDefault();
      submit(false);
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

<div
  class="dock"
  bind:this={root}
  role="dialog"
  aria-label="the prompt"
  tabindex="-1"
  onkeydown={onkey}
>
  {#if ask.kind === 'permission'}
    <div class="d-head">
      <span class="tag warn">&#9888; needs approval</span>
      <span class="q mono">{ask.request.title}</span>
    </div>
    {#if ask.request.subject !== ''}
      <!-- The subject exactly as it arrives: prose when Claude sent prose, a
           command when it did not. The wire's own pick between the two. -->
      <div class="d-q">{ask.request.subject}</div>
    {/if}
    {#if ask.request.reason !== null}
      <div class="reason">{ask.request.reason}</div>
    {/if}
    {#if ask.request.description !== null}
      <div class="desc">{ask.request.description}</div>
    {/if}
    {#if reason !== null}
      <div class="custom" class:live={take !== null}>
        {#if take !== null}
          <TakeCard {take} {wire} oncancel={onabandon} />
        {:else}
          <Field
            editor="dock"
            class="notes"
            bind:value={notes}
            rows={1}
            placeholder={reason.name}
            onkeydown={(event: KeyboardEvent) => {
              event.stopPropagation();
              onkey(event);
            }}
            field={(el: HTMLElement | null) => {
              field = el;
            }}
          />
        {/if}
        {#if dictation}
          <button
            class="micb"
            type="button"
            aria-label={take === null ? 'dictate the answer' : 'stop dictating'}
            onclick={() => {
              // **Pressing the mic is being in the row.** It counts as use
              // whether or not the caret follows it - without it the words a
              // take lands would go to the composer's draft, which is not
              // drawn while a prompt holds the slot, so the phone's only way
              // to dictate an answer would lose the answer.
              visited = true;
              onmic();
            }}
          >
            <Icon name="mic" />
          </button>
        {/if}
      </div>
    {/if}
    <div class="acts">
      {#each actions as option (option.optionId)}
        <button
          class="btn"
          class:d={option.kind === 'deny'}
          class:p={option.optionId === primary}
          onclick={() => decide(option.kind === 'deny' ? denyWith : option)}
        >
          {option.kind === 'deny' && notes.trim() !== '' && reason !== null
            ? 'Deny with these words'
            : option.name}
        </button>
      {/each}
    </div>
  {:else if ask.kind === 'question'}
    <div class="d-head">
      <span class="qm"><Icon name="question" /></span>
      <span class="d-q">{ask.request.question}</span>
      {#if ask.request.total > 1}
        <!-- The batch as segments rather than a sentence: a count of three is
             something to see, not to read - and the position is said in words
             beside it, because a row of marks says nothing to a screen reader. -->
        <span class="segs" aria-hidden="true">
          {#each Array.from({ length: ask.request.total }, (_, at) => at) as at (at)}
            <i class:on={at === ask.request.index}></i>
          {/each}
        </span>
        <span class="sr">question {ask.request.index + 1} of {ask.request.total}</span>
      {/if}
    </div>
    <div class="desc">{ask.request.header}</div>
  {:else}
    <div class="d-head">
      <span class="dest">
        {ask.request.workspace} · {ask.request.conversationLabel}{#if ask.request.threadTs !== null}
          · thread {ask.request.threadTs}{/if}
      </span>
      <span class="q mono">
        {ask.request.threadTs === null ? 'Post to Slack' : 'Reply in Slack'}
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
          <!-- The row's own number, which is the key that answers it. -->
          <span class="n" aria-hidden="true">{at + 1}</span>
          {#if !row.custom}
            {#if multi}
              <span class="box2" class:on={row.optionId !== null && toggled.includes(row.optionId)}>
                {#if row.optionId !== null && toggled.includes(row.optionId)}
                  <Icon name="check" />
                {/if}
              </span>
            {:else}
              <!-- A single-answer question marks its rows in the same slot the
                   set draws its boxes in - a circle that fills on the row being
                   taken - so its options are not bare against every other kind
                   of row on the page. -->
              <span class="radio" class:on={at === marked}></span>
            {/if}
          {/if}
          <span class="tx">
            <span class="lbl">{row.label}</span>
            {#if row.detail !== null}
              <span class="why">{row.detail}</span>
            {/if}
          </span>
          {#if answered && answeredRow === at}
            <!-- The wait rides the row that was answered. -->
            <span class="answering"><span class="ring"></span>sending</span>
          {:else if at === marked}
            <span class="kk"><kbd>Enter</kbd></span>
          {/if}
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
  {/if}

  {#if question}
    <!-- The one row that takes words, which is also the row a take's words
         land in. It is always drawn rather than revealed by a mark: it is the
         answer for every kind of prompt, not an escape hatch for one. -->
    <div class="custom" class:live={take !== null}>
      {#if take !== null}
        <TakeCard {take} {wire} oncancel={onabandon} />
      {:else}
        <Field
          editor="dock"
          class="notes"
          bind:value={notes}
          rows={1}
          placeholder="Or tell the agent something else&#8230;"
          onkeydown={(event: KeyboardEvent) => {
            event.stopPropagation();
            onkey(event);
          }}
          field={(el: HTMLElement | null) => {
            field = el;
          }}
        />
      {/if}
      {#if dictation}
        <button
          class="micb"
          type="button"
          aria-label={take === null ? 'dictate the answer' : 'stop dictating'}
          onclick={() => {
            // **Pressing the mic is being in the row.** It counts as use
            // whether or not the caret follows it - without it the words a take
            // lands would go to the composer's draft, which is not drawn while
            // a prompt holds the slot, so the phone's only way to dictate an
            // answer would lose the answer.
            visited = true;
            onmic();
          }}
        >
          <Icon name="mic" />
        </button>
      {/if}
      {#if take === null && notes.trim() !== ''}
        <button
          class="sendb"
          type="button"
          aria-label="send what you wrote as the answer"
          onclick={() => submit(true)}
        >
          <Icon name="send" />
        </button>
      {/if}
    </div>
  {/if}

  {#if ask.kind === 'slack_draft'}
    <div class="acts">
      <button class="btn d" onclick={() => post(false)}>Don't send</button>
      <button class="btn p" onclick={() => post(true)}>Post</button>
    </div>
    {#if take !== null}
      <!-- A take still running behind a held post: this branch draws no words
           row, so without this line a prompt would swallow a recording that is
           still going. -->
      <div class="blip">
        <span class="dot" class:tr={take.phase === 'transcribing'}></span>
        dictating · the words land in your draft either way
      </div>
    {/if}
  {/if}

  {#if notice !== null}
    <!-- A refusal is a row of its own, in the tone of a thing gone wrong. -->
    <div class="refusal">{notice}</div>
  {/if}

  {#if takeline !== null}
    <!-- What became of a take this page started, drawn here rather than in the
         box's own notice row: that row is not on screen while a prompt holds
         the slot, and the line would be invisible exactly when the reader
         pressed the mic that produced it. -->
    <div class="refusal">{takeline.text}</div>
  {/if}

  {#if answered && answeredRow === null}
    <!-- The same mark the reader's own words carry while they are on their
         way, because it is the same wait: an answer that has left the reader
         and has not been taken yet. A wait with a row to ride draws there; an
         answer from the words has none, and neither have the kinds that draw
         buttons. -->
    <div class="keys">
      <span class="answering"
        ><span class="ring"></span>sending · it holds until the core takes it</span
      >
    </div>
  {:else if !answered}
    <div class="keys">
      {#if question}
        <span><kbd>↑</kbd><kbd>↓</kbd> move</span>
        {#if multi}<span><kbd>space</kbd> toggle</span>{/if}
        <span><kbd>1</kbd>&ndash;<kbd>9</kbd> pick</span>
        <span><kbd>Enter</kbd> {submitLabel}</span>
      {:else}
        <span><kbd>Tab</kbd> between the actions</span>
        <span><kbd>Enter</kbd> activates</span>
      {/if}
      {#if take !== null}
        <span><kbd>Esc</kbd> cancels the take, then rejects</span>
      {:else}
        <span><kbd>Esc</kbd> reject</span>
      {/if}
    </div>
  {/if}

  {#if depth > 1}
    <div class="queue">{depth - 1} more pending after this</div>
  {/if}
</div>
