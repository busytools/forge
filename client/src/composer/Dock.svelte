<script lang="ts">
  import Prose from '../chat/Prose.svelte';
  import Icon from '../components/Icon.svelte';
  import {
    attachHandOff,
    clearHandOff,
    closeSurface,
    openSurface,
    surface,
  } from '../browser/surface.svelte';
  import Field from './Field.svelte';
  import TakeCard from './TakeCard.svelte';
  import type { Command } from '../protocol';
  import { goTo } from '../routes';
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
    silent = null,
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
     * words row draws, on the same terms the composer's card draws it.
     */
    wire?: {
      frames: number;
      bytes: number;
      rate: number | null;
      dbfs: number[];
      elapsedMs: number;
    } | null;
    /**
     * The mic on the words row: this surface's own door to a take.
     *
     * A divergence from the composer's "the mic is the door, not the trigger",
     * and the reason is rule 22's touch door: the push-to-talk key does not
     * exist on a phone, so a row that could only SHOW a take would leave a
     * finger no way to dictate an answer at all.
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
     * What the row says when the device is feeding this page's capture
     * nothing, read by the composer - which owns the take - rather than here.
     */
    silent?: string | null;
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
     * **The dock stands where the view this client replaced popped its
     * prompt**, and the mark says so: the pick is drawn at once and everything
     * else on the dock stands down, so a second Enter is not read as a second
     * answer to a prompt the reader has already answered. A refusal brings it
     * back live, with the reason.
     */
    answered?: boolean;
  } = $props();

  /**
   * The order a permission's actions are drawn in: **the approving action
   * leads** (Ved, 2026-10-10 - the settled design draws Allow always, Allow
   * once, Deny), then edit, and the refusal last.
   */
  const RANK: Record<PermissionOption['kind'], number> = { allow: 0, edit: 1, notes: 2, deny: 3 };

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
  /**
   * The ONE action drawn primary: the last trusting one the wire sent, which
   * is the wire's own "Allow always" where it offers both.
   *
   * Not every allow: the wire's common prompt offers two of them, and marking
   * both is no hierarchy at all.
   */
  const preferredAllow = $derived(
    ask.kind === 'permission'
      ? ([...ask.request.options].reverse().find((option) => option.kind === 'allow')?.optionId ??
          null)
      : null,
  );
  const actions = $derived(
    ask.kind === 'permission'
      ? [...ask.request.options]
          .filter((option) => option.kind !== 'notes')
          // **The approving action leads** (the settled design draws Allow
          // always, Allow once, Deny): the trusting actions first, the one
          // drawn primary first among them, and the refusal last.
          .sort((a, b) => {
            const rank = (RANK[a.kind] ?? 9) - (RANK[b.kind] ?? 9);
            if (rank !== 0) return rank;
            return (
              (a.optionId === preferredAllow ? 0 : 1) - (b.optionId === preferredAllow ? 0 : 1)
            );
          })
      : [],
  );
  /** The permission's own-words option, when the wire offers one. */
  const reason = $derived(
    ask.kind === 'permission'
      ? (ask.request.options.find((option) => option.kind === 'notes') ?? null)
      : null,
  );
  /** The ONE action drawn primary, which is the one the order leads with. */
  const primary = $derived(preferredAllow);

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
      // **The options as the wire sent them, and nothing else.** The words row
      // below the list is the answer for every kind of prompt; it is not an
      // option, so it is not a row.
      return prompt.request.options.map((option) => ({
        key: option.optionId,
        label: option.label,
        detail: option.description,
        preview: option.preview,
        optionId: option.optionId,
      }));
    }
    return [];
  }

  /** Which row a key would take, which is the first until one moves it. */
  let marked = $state(0);

  /**
   * Whether this prompt draws the words row, which is the LAST row a key can
   * take: the options end at `rows.length`, and one more step lands in the
   * field with the caret in it (the settled design - the row a reader arrives
   * on IS the input, and typing needs no extra press).
   */
  const hasWords = $derived(
    ask.kind === 'question' || (ask.kind === 'permission' && reason !== null),
  );

  /**
   * Whether the mark stands on the words row rather than on an option.
   *
   * Only where there IS a ladder: a prompt with no option rows (a permission)
   * keeps its own keyboard - the dock, then Tab to the buttons - so the words
   * row never claims a mark it was not walked onto.
   */
  const onWords = $derived(hasWords && rows.length > 0 && marked >= rows.length);

  /** Whether the body is folded to the bar, which the reader toggles. */
  let collapsed = $state(false);
  /** The folded bar, so the keyboard has somewhere to land when it folds. */
  let barEl = $state<HTMLButtonElement | null>(null);

  // **The keyboard follows the fold.** Folding unmounts the head's control, so
  // the caret would land on the body and the bar is the only route back. Keyed
  // on the bar element as well as the flag: it does not exist yet when
  // `collapsed` flips, and binding is what makes this run again and take.
  // Unfolding is covered by the arrival effect above (the listbox remounts).
  $effect(() => {
    if (!collapsed) return;
    barEl?.focus();
  });
  /** The options a multi-select question has toggled, in the order they were. */
  let toggled = $state<string[]>([]);

  /** What the submit key says, with the count riding it so a set is never sent unseen. */
  const submitLabel = $derived(
    multi && toggled.length > 0 ? `submit ${String(toggled.length)}` : 'submit',
  );
  /** The field the words row draws, which is also where a take's words land. */
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
    // **While folded the bar owns the caret.** The listbox unmounts with the
    // body, which re-runs this effect with `listbox` null - and without this
    // guard the root takes the focus the bar's own effect just placed.
    if (collapsed) return;
    if (inFieldNow()) return;
    // The options where there are any, and the dock itself where there are not
    // (a permission, a held post) - NOT the words row: the options are what
    // the arrival is for, and the row is entered by the reader's own doing.
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

  /** Whether the caret is in the words row's field right now. */
  function inFieldNow(): boolean {
    return field !== null && field.contains(document.activeElement);
  }

  // Whether the reader has been in the words row - set by writing in it, by a
  // mic press, by the row - which is sticky rather than tied to the caret, and
  // is what the keyboard table reads for this surface. It is NOT set by the
  // row merely being drawn: a prompt nobody has touched belongs to its
  // options, and the caret-return below is for a reader who was writing.
  //
  // **The mark follows the caret into the row**, so the keys and the drawn
  // state agree on where the reader is: a click into the field (or a landed
  // take's words) puts the mark on the words row, exactly as an arrow down
  // onto it puts the caret in the field.
  //
  // The cleanup is load-bearing: a seat can fail with a prompt still waiting,
  // which takes this dock off screen while the prompt stays, and a `true` left
  // standing here would keep reporting a box that is no longer there.
  $effect(() => {
    const el = field;
    if (el === null) return;
    const on = (): void => {
      visited = true;
      if (hasWords && !answered) marked = rows.length;
    };
    el.addEventListener('focus', on);
    if (el === document.activeElement) on();
    return () => {
      el.removeEventListener('focus', on);
    };
  });

  // The "used" flag itself, which outlives the field: a take drawing in the row
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
      void connection.dispatch(command, slot);
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
    // A key from the LIST answers the row it is on; the words row is not a row,
    // so Enter there arrives as `fromField` and carries no option at all.
    if (!fromField && onWords) return;
    const row = fromField ? undefined : rows[marked];
    if (!fromField && row === undefined) return;

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
    const ids = multi && toggled.length > 0 ? toggled : row === undefined ? [] : [row.optionId];
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
        // reject in this dock: the view this client replaced rejected with
        // Escape, and Escape here belongs to a take and then to a permission.
        outcome: { outcome: 'answered', selected_option_ids: ids, annotation },
      },
    });
  }

  /**
   * Answer with the prompt's own way out.
   *
   * A question refuses with an outcome of its own rather than a row, which is
   * the shape the core offers and the one the view this client replaced sent -
   * a question whose options are all wrong has no row to say so with, and an
   * empty `answered` would pick nothing on the reader's behalf.
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
    if (ask.kind === 'browser_hand_off') {
      // A hand-off refuses with its own "not now", which is the answer the
      // core reads as the session carrying on without the browser. Without
      // this arm the parked call - which has no timeout - waits forever on a
      // key the dock's own row advertises.
      handOff(false);
      return;
    }
    if (ask.kind === 'document_hand_off') {
      // The same arm for the same reason: Not now is the answer the core
      // reads as the reader declining, and it is the only release for a
      // parked call with no timeout.
      declineDocument();
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

  /**
   * The hand-off's own header line, built here rather than in the markup: a
   * `{' · '}` interpolation beside an `{#if}` keeps its spacing - Svelte
   * trims the block's own edges - and is a literal a lint (rightly) calls
   * useless; one template string says it once.
   */
  const handOffTitle = $derived(
    ask.kind === 'browser_hand_off' && ask.request.profile !== null
      ? `browser hand-off · ${ask.request.profile}`
      : 'browser hand-off',
  );
  /** Which hand-off's view Open was asked for and did not open, by its id. */
  let raiseFailedFor = $state<string | null>(null);
  /** Why the raise failed, in the shell's own words - a machine with no
   *  browser to drive is told which one to install. */
  let raiseWhy = $state<string | null>(null);
  const raiseFailed = $derived(
    ask.kind === 'browser_hand_off' && raiseFailedFor === ask.request.id
      ? (raiseWhy ??
          'The browser could not be opened - act where the browser is up, then press Done.')
      : null,
  );

  /**
   * Raise the browser surface, which is the client's own act rather than the
   * core's.
   *
   * **The takeover, not a separate window**: the app runs its own CEF host
   * over the profile the agents drive, and Open puts that page over the whole
   * window at full screen - nothing is relaunched, so the page the session
   * prepared is the page that comes up. An open that does not land says so
   * rather than claiming the browser is up.
   */
  function open(): void {
    if (ask.kind !== 'browser_hand_off') return;
    const id = ask.request.id;
    // **The takeover marks itself live for this ask, and the composer's bar
    // is what answers it.** Attaching also routes a take started from the
    // bar's mic into this dock's own words box.
    attachHandOff({ id, profile: ask.request.profile });
    // **The hand-off's own profile, when it named one**: the takeover draws
    // THAT profile's page - the CAPTCHA lands in the session the person
    // needs to act in. **The asking seat rides the raise**: the shell
    // refuses a profile another seat owns, so a hand-off cannot surface a
    // browser holding someone else's logins.
    void openSurface(ask.request.profile, slot).then((why) => {
      if (why !== null) {
        raiseFailedFor = id;
        raiseWhy = why;
      }
    });
  }

  /** A hand-off's answer, which is the only release for its blocked handler.
   *  **Answering also lowers the takeover**: Open covers the chat, Done or
   *  Not now gives it back, and the browser keeps running with its page and
   *  tabs intact (Ved, 2026-10-07: "I said done, but the browser is still
   *  open" - he stops seeing it, and the next call drives it). */
  function handOff(done: boolean): void {
    if (answered || ask.kind !== 'browser_hand_off') return;
    const id = ask.request.id;
    // **Read before the answer.** Answering clears the dock's own ask, so a
    // profile read after the command crosses throws and the window never
    // comes down.
    const profile = ask.request.profile;
    onanswer(id);
    answer({ respond_browser_hand_off: { key: slot, id, done } });
    void closeSurface(profile);
    clearHandOff(id);
  }

  /**
   * Open the handed document: the client's own route, and no answer - the
   * parked call releases on the viewer's own done, so nothing is told the
   * document was read until it was.
   */
  function openDocument(): void {
    if (ask.kind !== 'document_hand_off') return;
    goTo({
      name: 'doc',
      slot,
      path: ask.request.path,
      reviewable: ask.request.reviewable,
      handoff: ask.request.id,
    });
  }

  /** Not now: the answer the core reads as the reader declining, which is
   *  the parked call's only other release. */
  function declineDocument(): void {
    if (answered || ask.kind !== 'document_hand_off') return;
    onanswer(ask.request.id);
    answer({ answer_document_hand_off: { key: slot, id: ask.request.id, answer: 'not_now' } });
  }

  /**
   * The phone's takeover bar, the same answer: its Done arrives as a window
   * event (the Kotlin bar calls into this page - see `installTakeoverHook`),
   * and this dock's own `handOff` is what answers it, refusals and all. Its
   * back never crosses: lowering is the host's, and an unanswered hand-off
   * simply stays parked.
   */
  $effect(() => {
    const onTakeover = (event: Event): void => {
      const what = (event as CustomEvent<string>).detail;
      // **The phone's back, the hardware Back and its bar's lower** all end
      // with the native takeover gone; the page's store follows, or a later
      // Open would draw over nothing.
      if (what === 'lowered') {
        surface.phase = 'closed';
        surface.profile = null;
        surface.reason = null;
        if (surface.hand_off !== null) clearHandOff(surface.hand_off.id);
        return;
      }
      if (what !== 'done') return;
      handOff(true);
    };
    window.addEventListener('forge-takeover', onTakeover);
    return () => window.removeEventListener('forge-takeover', onTakeover);
  });

  function move(step: number): void {
    // An answered prompt is standing down: its rows are a record of what was
    // answered, not a list to move through.
    const span = rows.length + (hasWords ? 1 : 0);
    if (span === 0 || answered) return;
    marked = (marked + step + span) % span;
    // **The keyboard follows the mark**, and one step past the last option
    // lands it in the words row's field with the caret ready - the coming-to
    // of the settled design's "typing needs no extra press".
    if (onWords) field?.focus();
    else listbox?.focus();
  }

  /**
   * **The capped list is a scroll container, so the mark has to be brought
   * into view.** Arrowing past the cap moved the mark to a row nobody could see,
   * and Enter answered it - worst on a phone, where the keyboard eats the
   * height the cap reserved. `nearest` is the slide that does nothing while
   * the row is already in view; a row on the words side scrolls itself into
   * view when the focus lands in it. jsdom implements no `scrollIntoView`, so
   * the call is guarded rather than fired blindly.
   */
  $effect(() => {
    if (onWords) return;
    const el = document.getElementById(rowId(marked));
    if (el === null || typeof el.scrollIntoView !== 'function') return;
    el.scrollIntoView({ block: 'nearest' });
  });

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
    if (!multi || answered || row === undefined) return;
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
      // which is the same rule the view this client replaced applied to the
      // same slot.
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
        // it is back among the choices rather than stuck writing. (`move`
        // settles the keyboard where the mark landed.)
        event.preventDefault();
        move(event.key === 'ArrowDown' ? 1 : -1);
        return;
      }
      if (event.key === 'Escape') {
        // A question's way out from here is the reject, the same behaviour the
        // view this client replaced had: its notes box cancelled the prompt.
        // The words typed so far go with it, because the prompt does.
        if (question) {
          event.preventDefault();
          reject();
          return;
        }
        // A permission steps back out of the field instead, so the words typed
        // so far stay in the state - a divergence from the view this client
        // replaced, which cancelled from its notes box for this kind too.
        event.preventDefault();
        if (rows.length > 0) move(-1);
        else root?.focus();
      }
      return;
    }
    if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
      event.preventDefault();
      // The keyboard follows the mark: an arrow down past the last option lands
      // in the words row with the caret in it, and an arrow back up returns to
      // the list (`move` settles both).
      move(event.key === 'ArrowDown' ? 1 : -1);
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
      // when it is clicked. The number past the last option is the words row,
      // which the row itself draws, so its key lands the caret there too.
      const at = Number(event.key) - 1;
      if (at < rows.length) {
        event.preventDefault();
        choose(at);
      } else if (at === rows.length && hasWords && !answered) {
        event.preventDefault();
        marked = at;
        field?.focus();
      }
      return;
    }
    if (event.key.length === 1 && !event.ctrlKey && !event.metaKey && !event.altKey) {
      // A printable character is the reader typing their own answer - the ARIA
      // practice's rule for a list with an input beside it: the mark moves to
      // the words row, the caret goes with it, and the character lands there.
      // A reader who starts typing never has to discover the row first.
      // (Digits answered above, and space toggles.)
      if (hasWords && !answered) {
        event.preventDefault();
        marked = rows.length;
        notes += event.key;
        field?.focus();
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
  {#snippet wordsRow()}
    <!-- **The words row is the prompt's last row, and it IS the input.** One
         step past the last option (or a typed character) lands the caret in
         it; what is written goes as the answer, and a take's words land in
         the card the composer draws. There is no second words affordance. -->
    <div class="opt qrow" class:on={onWords}>
      {#if question}
        <span class="n" aria-hidden="true">{rows.length + 1}</span>
      {/if}
      {#if take !== null}
        <!-- A take the dock watches is one the record already has: its own
             phase is all there is to say about it here. -->
        <TakeCard {take} phase={take.phase} {wire} oncancel={onabandon} {silent} spread />
      {:else}
        <Field
          editor="dock"
          class="notes"
          bind:value={notes}
          rows={1}
          placeholder={reason !== null ? reason.name : 'Tell the agent something else…'}
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
            // whether or not the caret follows it, so the caret comes back to
            // the words row when the take's card leaves - which a touch screen
            // cannot reach any other way.
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
  {/snippet}

  {#if collapsed}
    <!-- The bar: the question and the count behind it, and that is all - a
         click (or Enter/Space on it) unfolds the body so the reader can see
         the page the question is about. **A real button, not a div with a key
         handler**: the root's own guard hands a button its keys, so the Enter
         that unfolds can never ALSO reach the dock's handler and answer the
         marked row (the accidental-answer defect this was drawn around). A
         take keeps drawing under it either way. -->
    <button
      class="qbar"
      type="button"
      aria-expanded="false"
      bind:this={barEl}
      onclick={() => {
        collapsed = false;
      }}
    >
      <span class="qm">?</span>
      <span class="t">{ask.kind === 'question' ? ask.request.question : ''}</span>
      {#if depth > 1}<span class="cnt">+{depth - 1}</span>{/if}
      <span class="chevc" aria-hidden="true">&#9652;</span>
    </button>
    {#if take !== null}{@render wordsRow()}{/if}
  {:else}
    <!-- The waiting prompts lead the EXPANDED lane only (Ved, 2026-10-10):
         knowing there are more behind this one is part of reading it, and the
         folded bar carries the count in its own chip instead of this line. -->
    {#if depth > 1}
      <div class="queue">&darr; {depth - 1} more pending after this</div>
    {/if}
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
        {@render wordsRow()}
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
        <span class="headend">
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
          <!-- Folding is the reader's: the bar is the way back. -->
          <button
            class="chevb"
            type="button"
            aria-label="fold the question to its bar"
            onclick={() => {
              collapsed = true;
            }}
          >
            <Icon name="down" class="ic" />
          </button>
        </span>
      </div>
      <div class="desc">{ask.request.header}</div>
    {:else if ask.kind === 'slack_draft'}
      <div class="d-head">
        <span class="dest">
          {ask.request.workspace} · {ask.request
            .conversationLabel}{#if ask.request.threadTs !== null}
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
    {:else if ask.kind === 'document_hand_off'}
      <div class="d-head">
        <span class="tag warn">&#9888; needs you</span>
        <span class="q mono">document hand-off</span>
      </div>
      <!-- The resolved path verbatim: it is the document's own address, and a
         basename would be the client guessing at which file was meant. -->
      <div class="d-q">{ask.request.path}</div>
      <div class="desc">
        Open reads it in the viewer{ask.request.reviewable
          ? ', where the session asked for comments'
          : ''}; the session waits until you finish. Not now tells it you are not reading this one.
      </div>
    {:else}
      <div class="d-head">
        <span class="tag warn">&#9888; needs you</span>
        <span class="q mono">{handOffTitle}</span>
      </div>
      <!-- The reason verbatim: it is what the session needs, in its own words,
         and a summary of it would be the client guessing at the act. -->
      <div class="d-q">{ask.request.reason}</div>
      <div class="desc">
        {raiseFailed ??
          'Open browser shows the page the session prepared, exactly as it stands. The session waits, with no timeout, until Done or Not now.'}
      </div>
    {/if}

    {#if rows.length > 0}
      <div
        class="opts"
        role="listbox"
        aria-label="the prompt's options"
        bind:this={listbox}
        tabindex="-1"
        aria-activedescendant={onWords ? undefined : rowId(marked)}
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
            {#if multi}
              <span class="box2" class:on={toggled.includes(row.optionId)}>
                {#if toggled.includes(row.optionId)}
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
      {@render wordsRow()}
    {/if}

    {#if ask.kind === 'slack_draft'}
      <div class="acts">
        <button class="btn p" onclick={() => post(true)}>Post</button>
        <button class="btn d" onclick={() => post(false)}>Don't send</button>
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

    {#if ask.kind === 'browser_hand_off'}
      <div class="acts">
        <!-- **The takeover carries the actions once it is up.** Open is the
           client's own act, not the core's: it replaces the window with the
           browser the sessions drive and answers nothing; Done, Not now, the
           feedback line and back-to-chat live on the takeover's own bar.
           Before it opens, the card carries Open, Done and Not now - the
           session waits with no timeout, so a way out must be reachable. -->
        {#if surface.phase === 'live' && surface.profile === ask.request.profile}
          <span class="fo">the browser is open over the window</span>
        {:else}
          <!-- **The card's three, as Ved asked (2026-10-19):** Open browser,
               Done and Not now before the takeover; inside it the bar adds
               the feedback line and back to chat. -->
          <button class="btn p" onclick={open}>Open browser</button>
          <button class="btn" onclick={() => handOff(true)}>Done</button>
          <button class="btn d" onclick={() => handOff(false)}>Not now</button>
        {/if}
      </div>
    {/if}

    {#if ask.kind === 'document_hand_off'}
      <div class="acts">
        <!-- Open routes to the viewer and answers nothing: the parked call
           releases on the viewer's own done, so the session hears what the
           reading produced. Not now is the answer. -->
        <button class="btn p" onclick={openDocument}>Open</button>
        <button class="btn d" onclick={declineDocument}>Not now</button>
      </div>
    {/if}

    {#if !answered}
      <div class="keys">
        {#if question}
          {#if onWords}
            <span><kbd>Enter</kbd> send these words</span>
            <span><kbd>&uarr;</kbd> back to the options</span>
            <span><kbd>Esc</kbd> reject</span>
          {:else}
            <span><kbd>&uarr;</kbd><kbd>&darr;</kbd> move</span>
            {#if multi}<span><kbd>space</kbd> toggle</span>{/if}
            <span><kbd>1</kbd>&ndash;<kbd>9</kbd> pick</span>
            <span><kbd>&darr;</kbd> past the last option: your own words</span>
            <span><kbd>Enter</kbd> {submitLabel}</span>
          {/if}
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
  {/if}
</div>
