<script lang="ts">
  import Prose from '../chat/Prose.svelte';
  import Icon from '../components/Icon.svelte';
  import { closeSurface, openSurface, surface } from '../browser/surface.svelte';
  import {
    closeTerminal,
    openTerminal,
    surface as terminalSurface,
  } from '../terminal/surface.svelte';
  import Field from './Field.svelte';
  import SendBack from './SendBack.svelte';
  import TakeCard from './TakeCard.svelte';
  import DictateControl from './DictateControl.svelte';
  import type { Dictation } from './dictate.svelte';
  import type { Command } from '../protocol';
  import { goTo } from '../routes';
  import { report, type Connection } from '../socket';
  import type { SessionSlot } from '../wire/types';
  import type {
    Ask,
    PermissionOption,
    SlackOperation,
    SlackPerson,
    SlackThreadName,
    Take,
  } from './wire';

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
    feedback = $bindable(''),
    feedbackOpen = $bindable(false),
    ownOpen = $bindable(false),
    land = null,
    feedbackLand = null,
    onanswer = () => {},
    onabandon = () => {},
    onsending = () => {},
    onmic = () => {},
    dictate = null,
    tookFrom = null,
    dictation = false,
    silent = null,
    takeline = null,
    answered = false,
    carried = null,
    visible = true,
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
    onmic?: (from: 'dock' | 'feedback') => void;
    /** The send-back box's own words, held by the composer like `notes`. */
    feedback?: string;
    /** Whether the send-back box is open, which makes it a destination. */
    feedbackOpen?: boolean;
    /** A take's words that landed in the send-back box, which come with the keyboard. */
    feedbackLand?: string | null;
    /** The dictation control both boxes wear, which the composer owns. */
    dictate?: Dictation | null;
    /** Which box the take began in, captured per take. */
    tookFrom?: 'dock' | 'composer' | 'feedback' | null;
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
     * A send-back has left the reader and is on its way: the card stands
     * down, so the line saying what is happening is drawn where it stood.
     */
    onsending?: () => void;
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
    /**
     * Words the takeover's line was holding when it was lowered, now in the
     * chat's draft. **The composer is behind this card while the hand-off is
     * parked**, so the card is where the person can read what happened to
     * them.
     */
    carried?: string | null;
    /**
     * Whether this dock is the body's drawn half.
     *
     * **The ask stays mounted while the reader's draft shows the field**, so
     * the toggled set and the marked row it holds are not lost to a remount;
     * the body hides it instead. What a hidden dock must not do is keep the
     * keyboard, and what a shown one owes is the keyboard back.
     */
    visible?: boolean;
  } = $props();

  /**
   * The order a permission's actions are drawn in: **the approving action
   * leads** (Ved, 2026-10-10 - the settled design draws Allow always, Allow
   * once, Deny), then edit, and the refusal last.
   */
  const RANK: Record<PermissionOption['kind'], number> = { allow: 0, edit: 1, notes: 2, deny: 3 };

  /**
   * A held draft's own verbs, by what it would do. One wording for all of
   * them is the defect this replaces: a delete card that says "post" is
   * asking the reader to approve the wrong act.
   */
  const SLACK_VERBS: Record<SlackOperation, { head: string; approve: string; decline: string }> = {
    post: { head: 'Post to Slack', approve: 'Post', decline: 'Do not post' },
    edit: { head: 'Edit in Slack', approve: 'Apply edit', decline: 'Do not apply' },
    delete: { head: 'Delete in Slack', approve: 'Delete', decline: 'Keep it' },
    react: { head: 'React in Slack', approve: 'React', decline: 'Do not react' },
    unreact: { head: 'Unreact in Slack', approve: 'Unreact', decline: 'Do not unreact' },
    upload: { head: 'Upload to Slack', approve: 'Upload', decline: 'Do not upload' },
    // A value from a newer server: say the least that is true rather than
    // claiming a post, which could approve an act nobody named.
    unknown: { head: 'Slack draft', approve: 'Approve', decline: 'Do not send' },
  };

  const slackVerbs = $derived.by(() => {
    if (ask.kind !== 'slack_draft') return { head: '', approve: '', decline: '' };
    const verbs = SLACK_VERBS[ask.request.operation];
    // Only a post leads with "Reply": an upload's thread is the destination's
    // stamp, not the card's verb.
    const reply = ask.request.operation === 'post' && ask.request.threadTs !== null;
    return { ...verbs, head: reply ? 'Reply in Slack' : verbs.head };
  });

  /**
   * The card's destination line: where it goes, the thread it names, and the
   * people it tags. A post's or an upload's stamp names the thread it replies
   * into; every other operation carries the target MESSAGE's ts in the same
   * field, and a destructive card must not call that a thread.
   *
   * **The thread is named core-side**, and a parent the core could not read
   * still reads as a thread rather than as a ts. The tags are people the core
   * resolved, and one it could not name keeps its id.
   */
  interface Dest {
    workspace: string;
    conversation: string;
    noun: 'thread' | 'message';
    threadTs: string | null;
    thread: SlackThreadName | null;
    tagged: SlackPerson[];
  }
  const slackDest = $derived.by<Dest>(() => {
    if (ask.kind !== 'slack_draft') {
      return {
        workspace: '',
        conversation: '',
        noun: 'thread',
        threadTs: null,
        thread: null,
        tagged: [],
      };
    }
    const { workspace, conversationLabel, threadTs, operation, thread, tagged } = ask.request;
    const noun = operation === 'post' || operation === 'upload' ? 'thread' : 'message';
    return { workspace, conversation: conversationLabel, noun, threadTs, thread, tagged };
  });

  /**
   * The draft's text as the card draws it: a mention the core named reads as
   * its name, as Slack draws it, and one it could not name keeps its id. Code
   * draws its own markup, so a mention inside a span or a fence stays as
   * written.
   */
  function namedBody(text: string, tagged: SlackPerson[]): string {
    const names = new Map(tagged.map((person) => [person.id, person.name]));
    const named = (mention: string, inner: string): string => {
      const id = inner.split('|')[0] ?? '';
      if (!/^[A-Za-z][A-Za-z0-9]*$/.test(id)) return mention;
      const name = names.get(id);
      return name === null || name === undefined ? id : `@${name}`;
    };
    let fenced = false;
    return text
      .split('\n')
      .map((line) => {
        if (/^\s*(```|~~~)/.test(line)) {
          fenced = !fenced;
          return line;
        }
        if (fenced) return line;
        return line
          .split(/(`[^`]*`)/)
          .map((part, at) => (at % 2 === 1 ? part : part.replace(/<@([^>]*)>/g, named)))
          .join('');
      })
      .join('\n');
  }

  const slackBody = $derived.by(() =>
    ask.kind === 'slack_draft' ? namedBody(ask.request.text, ask.request.tagged) : '',
  );

  /**
   * The two places the line yields are decided from MEASURED widths, because
   * a container query cannot know how long an excerpt or a tag list is.
   *
   * `tags-below` is the mock's crowded state: the thread takes the row and
   * the tags move under it. `pill-stacked` is the chip's: its two halves
   * stack only when they cannot share a line, so the `·` between them is not
   * hidden while they still read as one line (Ved's round-2 note).
   */
  let destEl = $state<HTMLSpanElement | null>(null);
  let tagsBelow = $state(false);
  let pillStacked = $state(false);

  /** A segment's one-line width, with a clipped child contributing its whole content. */
  function segmentWidth(segment: Element | null): number {
    if (!(segment instanceof HTMLElement)) return 0;
    const style = getComputedStyle(segment);
    const gap = Number.parseFloat(style.columnGap) || 0;
    const frame =
      (Number.parseFloat(style.paddingLeft) || 0) +
      (Number.parseFloat(style.paddingRight) || 0) +
      (Number.parseFloat(style.borderLeftWidth) || 0) +
      (Number.parseFloat(style.borderRightWidth) || 0);
    const children = [...segment.children];
    const width = children.reduce((sum, child) => {
      if (!(child instanceof HTMLElement)) return sum;
      if (child.classList.contains('ex')) {
        // The excerpt shares the thread's free space, so its laid-out width is
        // whatever the row gave it - not what the words need. Probe it at zero
        // width so the measurement does not read its own fill back.
        const maxWidth = child.style.maxWidth;
        const minWidth = child.style.minWidth;
        child.style.maxWidth = '0px';
        child.style.minWidth = '0px';
        const content = child.scrollWidth;
        child.style.maxWidth = maxWidth;
        child.style.minWidth = minWidth;
        return sum + content;
      }
      const clipped = child.scrollWidth > child.clientWidth;
      return sum + (clipped ? child.scrollWidth : child.offsetWidth);
    }, 0);
    return width + frame + gap * Math.max(0, children.length - 1);
  }

  $effect(() => {
    const el = destEl;
    // Keyed on the line's own content, so a new draft is measured again.
    void slackDest.workspace;
    void slackDest.conversation;
    void slackDest.threadTs;
    void slackDest.thread;
    void slackDest.tagged;
    if (el === null || typeof ResizeObserver === 'undefined') return;
    const measure = (): void => {
      const width = el.clientWidth;
      if (width === 0) return;
      const gap = Number.parseFloat(getComputedStyle(el).columnGap) || 0;
      const parts = [...el.children];
      const need =
        parts.reduce((sum, part) => {
          if (!(part instanceof HTMLElement)) return sum;
          // The tag separator is its own item; the thread's leading `·` is a
          // pseudo-element this sum cannot see.
          if (part.classList.contains('tsep')) return sum + 6;
          const leading = part.classList.contains('thr') ? 8 : 0;
          return sum + segmentWidth(part) + leading;
        }, 0) +
        gap * Math.max(0, parts.length - 1);
      tagsBelow = need > width;
      const ws = el.querySelector('.pin .ws');
      const ch = el.querySelector('.pin .ch');
      const pinNeed =
        (ws instanceof HTMLElement ? ws.scrollWidth : 0) +
        (ch instanceof HTMLElement ? ch.scrollWidth : 0) +
        40;
      pillStacked = pinNeed > width;
    };
    const observer = new ResizeObserver(measure);
    observer.observe(el);
    measure();
    return () => observer.disconnect();
  });

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
      // A held draft draws no rows either: it is approved, sent back, or
      // declined, and those verbs are buttons under the body they act on.
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

  // **The keyboard comes back with the dock.** Hiding it while the reader's
  // draft shows the field moves no state but does move the caret, so showing
  // it again gives the caret back to the bar or the rows they left.
  let wasVisible = true;
  $effect(() => {
    const shown = visible;
    if (shown && !wasVisible) {
      (collapsed ? barEl : (listbox ?? root))?.focus({ preventScroll: true });
    }
    wasVisible = shown;
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
    // (a permission, a held draft) - NOT the words row: the options are what
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

  // The send-back box's own landing: a take begun there focuses its field,
  // the same way the composer's does.
  $effect(() => {
    if (feedbackLand === null) return;
    if (feedbackEl !== null) feedbackEl.focus();
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
      handOff('not_now', 'dock_escape');
      return;
    }
    if (ask.kind === 'terminal_hand_off') {
      // The same arm for the same reason: a parked terminal hand-off has no
      // timeout, and Not now is the only release.
      terminalHandOff('not_now', 'dock_escape');
      return;
    }
    if (ask.kind === 'document_hand_off') {
      // The same arm for the same reason: Not now is the answer the core
      // reads as the reader declining, and it is the only release for a
      // parked call with no timeout.
      declineDocument();
      return;
    }
    if (ask.kind === 'settings_write') {
      // A held write refuses with its own decline, which is the answer the
      // core reads as nothing being written.
      write(false);
      return;
    }
    if (ask.kind === 'vault') {
      // A held vault call refuses with its own decline, which is the answer
      // the core reads as nothing reaching the vault.
      answerVault('decline');
      return;
    }
    // A held draft refuses with its own decline, which is the answer the
    // core reads as the draft going nowhere.
    post('decline');
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

  /** A held draft's verbs, which are buttons rather than rows. */
  function post(decision: 'approve' | 'decline'): void {
    if (answered || ask.kind !== 'slack_draft') return;
    const id = ask.request.id;
    onanswer(id);
    answer({ respond_slack_post: { key: slot, id, answer: decision } });
  }

  let feedbackEl = $state<HTMLElement | null>(null);
  let sendBackButton = $state<HTMLButtonElement | null>(null);

  // **The field arrives with the keyboard**, the same rule the words row
  // follows: opening it is the reader's press, so writing needs no second one.
  $effect(() => {
    if (feedbackOpen) feedbackEl?.focus();
  });

  function openFeedback(): void {
    if (answered || ask.kind !== 'slack_draft') return;
    feedbackOpen = true;
  }

  /** Escape closes the field without sending, and the button that opened it
   *  takes the keyboard back so the card is not left with nowhere to land. */
  function closeFeedback(): void {
    feedbackOpen = false;
    sendBackButton?.focus();
  }

  /**
   * Hand the draft back with the reader's words. Nothing is sent to Slack:
   * the blocked call reads the note and the agent is expected to revise.
   */
  function sendBack(): void {
    if (answered || ask.kind !== 'slack_draft') return;
    // Whitespace is not feedback: the action stays unavailable until the
    // field carries words, and this is the same guard for a key that
    // reaches here anyway.
    if (feedback.trim() === '') return;
    const id = ask.request.id;
    onanswer(id);
    onsending();
    answer({
      respond_slack_post: {
        key: slot,
        id,
        answer: { send_back: { note: feedback } },
      },
    });
  }

  /** The send-back field's own keys: Enter sends, Shift+Enter is the editor's
   *  newline, Escape closes without sending. */
  function onFeedbackKey(event: KeyboardEvent): void {
    event.stopPropagation();
    if (event.key === 'Enter' && !event.shiftKey) {
      event.preventDefault();
      sendBack();
      return;
    }
    if (event.key === 'Escape') {
      event.preventDefault();
      closeFeedback();
    }
  }

  /** A held configuration write's two verbs, on the post's own terms. */
  function write(approved: boolean): void {
    if (answered || ask.kind !== 'settings_write') return;
    const id = ask.request.id;
    onanswer(id);
    answer({ respond_settings_write: { key: slot, id, approved } });
  }

  /** A value as the write's two blocks draw it: text bare, everything JSON. */
  function showValue(value: unknown): string {
    if (typeof value === 'string') return value;
    return JSON.stringify(value, null, 2) ?? String(value);
  }

  /**
   * The vault card's unlock material, which rides only an approval.
   *
   * **The master password never enters the transcript**: it is typed in a
   * masked field here and travels in this answer alone, where the core hands
   * it to the unlock path. A send-back or a decline runs nothing, so it
   * carries neither.
   */
  let masterPassword = $state('');
  let twoFactor = $state('');

  /** Whether the vault's send-back field is open, and the note it holds. */
  let vaultNoteOpen = $state(false);
  let vaultNote = $state('');
  let vaultNoteEl = $state<HTMLElement | null>(null);
  let vaultSendBackButton = $state<HTMLButtonElement | null>(null);

  // **The field arrives with the keyboard**, the same rule the words row and
  // a held post's feedback field follow.
  $effect(() => {
    if (vaultNoteOpen) vaultNoteEl?.focus();
  });

  function openVaultNote(): void {
    if (answered || ask.kind !== 'vault') return;
    vaultNoteOpen = true;
  }

  /** Escape closes the field without sending, and the button that opened it
   *  takes the keyboard back so the card is not left with nowhere to land. */
  function closeVaultNote(): void {
    vaultNoteOpen = false;
    vaultSendBackButton?.focus();
  }

  /** The send-back field's own keys: Enter sends, Shift+Enter is the editor's
   *  newline, Escape closes without sending. */
  function onVaultNoteKey(event: KeyboardEvent): void {
    event.stopPropagation();
    if (event.key === 'Enter' && !event.shiftKey) {
      event.preventDefault();
      answerVault('send_back');
      return;
    }
    if (event.key === 'Escape') {
      event.preventDefault();
      closeVaultNote();
    }
  }

  /** A locked card's fields: Enter approves and Escape hands the keyboard
   *  back to the card, so a reader is never stuck inside a masked box. */
  function onUnlockKey(event: KeyboardEvent): void {
    event.stopPropagation();
    if (event.key === 'Enter') {
      event.preventDefault();
      answerVault('approve');
      return;
    }
    if (event.key === 'Escape') {
      event.preventDefault();
      answerVault('decline');
    }
  }

  /**
   * A held vault call's verbs: approve runs the call, send-back hands it to
   * the agent with the reader's words, and decline runs nothing.
   *
   * **A locked card cannot approve without the master password**, because
   * the approval is what performs the unlock; a card whose session is already
   * unlocked sends no unlock material at all.
   */
  function answerVault(decision: 'approve' | 'send_back' | 'decline'): void {
    if (answered || ask.kind !== 'vault') return;
    if (decision === 'approve' && ask.request.needsUnlock && masterPassword === '') return;
    if (decision === 'send_back' && vaultNote.trim() === '') return;
    const id = ask.request.id;
    const code = twoFactor.trim();
    const unlock =
      decision === 'approve' && ask.request.needsUnlock
        ? { master_password: masterPassword, ...(code === '' ? {} : { two_factor: code }) }
        : {};
    const outcome = decision === 'send_back' ? { decision, note: vaultNote } : { decision };
    onanswer(id);
    if (decision === 'send_back') onsending();
    answer({ respond_vault_ask: { key: slot, id, answer: { ...outcome, ...unlock } } });
  }

  /**
   * The hand-off's own header line, built here rather than in the markup: a
   * `{' · '}` interpolation beside an `{#if}` keeps its spacing - Svelte
   * trims the block's own edges - and is a literal a lint (rightly) calls
   * useless; one template string says it once.
   */
  const handOffTitle = $derived.by((): string => {
    if (ask.kind === 'terminal_hand_off') {
      return ask.request.terminal === null
        ? 'terminal hand-off'
        : `terminal hand-off · terminal ${ask.request.terminal}`;
    }
    return ask.kind === 'browser_hand_off' && ask.request.tab !== null
      ? `browser hand-off · tab ${ask.request.tab}`
      : 'browser hand-off';
  });
  /**
   * What a folded bar says: the kind's mark and its subject. **Every kind
   * folds**, not only the question - the body is one slot, so any ask can give
   * it back to the field for a look, and the bar is the way in and out.
   */
  const barMark = $derived.by(() => {
    if (ask.kind === 'question') return 'question';
    if (ask.kind === 'permission') return 'warning';
    if (ask.kind === 'slack_draft') return 'plane';
    if (ask.kind === 'terminal_hand_off') return 'code';
    if (ask.kind === 'document_hand_off') return 'file';
    if (ask.kind === 'settings_write') return 'settings';
    if (ask.kind === 'vault') return 'decide';
    return 'bell';
  });
  const barText = $derived.by(() => {
    if (ask.kind === 'question') return ask.request.question;
    if (ask.kind === 'permission') {
      return ask.request.subject === ''
        ? ask.request.title
        : `${ask.request.title} · ${ask.request.subject}`;
    }
    if (ask.kind === 'slack_draft') {
      return `${ask.request.workspace} · ${ask.request.conversationLabel}`;
    }
    if (ask.kind === 'document_hand_off') return ask.request.path;
    if (ask.kind === 'settings_write') {
      return ask.request.edits.map((edit) => edit.key).join(', ');
    }
    if (ask.kind === 'terminal_hand_off') {
      return ask.request.terminal === null
        ? 'terminal hand-off'
        : `terminal hand-off · terminal ${ask.request.terminal}`;
    }
    if (ask.kind === 'vault') return ask.request.summary;
    return handOffTitle;
  });
  /** Which hand-off's view Open was asked for and did not open, by its id. */
  let raiseFailedFor = $state<string | null>(null);
  /** Why the raise failed, in the shell's own words - a machine with no
   *  browser to drive is told which one to install. */
  let raiseWhy = $state<string | null>(null);
  const raiseFailed = $derived.by((): string | null => {
    if (raiseFailedFor === null) return null;
    const id =
      ask.kind === 'browser_hand_off' || ask.kind === 'terminal_hand_off' ? ask.request.id : null;
    if (id === null || raiseFailedFor !== id) return null;
    if (raiseWhy !== null) return raiseWhy;
    return ask.kind === 'browser_hand_off'
      ? 'The browser could not be opened - act where the browser is up, then press Done.'
      : 'The terminal could not be opened - read the screen from the session and try again.';
  });

  /**
   * Raise the browser surface, which is the client's own act rather than the
   * core's.
   *
   * **The takeover, not a separate window**: the SERVER runs the CEF host
   * over the profile the agents drive, and Open attaches this client as a
   * viewer to the tab the hand-off names, over the whole window under the
   * bar. **No tab outlives its host**: a profile whose host had closed comes
   * back with the session's next tab, and an ask naming a tab that is gone
   * is refused by name rather than shown. An open that does not land says so
   * rather than claiming the browser is up.
   */
  function open(): void {
    if (ask.kind !== 'browser_hand_off') return;
    const id = ask.request.id;
    // **The hand-off's own page**: the takeover draws THAT tab's page - the
    // CAPTCHA lands in the page the session prepared. **The asking seat rides
    // the raise**, and the server resolves the seat's own profile. A page id
    // is what opens it, so a host replacement under the parked ask cannot
    // raise whatever took the tab's number.
    void openSurface(null, slot, ask.request.tab, ask.request.target).then((why) => {
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
   *  open" - he stops seeing it, and the next call drives it).
   *
   *  **The source rides the answer** so the core's log names the door the
   *  ending came through, not just its meaning. */
  function handOff(ending: 'done' | 'not_now', source: 'dock' | 'dock_escape'): void {
    if (answered || ask.kind !== 'browser_hand_off') return;
    const id = ask.request.id;
    onanswer(id);
    answer({
      respond_browser_hand_off: { key: slot, id, answer: { ending: { type: ending }, source } },
    });
    // The lower names no profile: the shell lowers the attach this page
    // actually holds, which is the one the hand-off's Open raised.
    void closeSurface(null, slot);
  }

  /**
   * Raise the seat's terminal for a parked hand-off, which is the client's own
   * act rather than the core's. The ask stays parked; the person's keystrokes
   * go to the PTY and nowhere else.
   */
  function openTerminalHandOff(): void {
    if (ask.kind !== 'terminal_hand_off') return;
    const terminal = ask.request.terminal;
    if (terminal === null) return;
    const id = ask.request.id;
    void openTerminal(slot, terminal).then((why) => {
      if (why !== null) {
        raiseFailedFor = id;
        raiseWhy = why;
      }
    });
  }

  /**
   * A terminal hand-off's answer, the only release for its blocked handler.
   * Answering also lowers the surface: the terminal keeps running with its
   * screen and scrollback intact, and the next raise attaches again.
   */
  function terminalHandOff(
    ending: 'done' | 'not_now' | 'feedback',
    source: 'dock' | 'dock_escape' | 'takeover' | 'feedback',
  ): void {
    if (answered || ask.kind !== 'terminal_hand_off') return;
    const id = ask.request.id;
    onanswer(id);
    answer({
      respond_terminal_hand_off: { key: slot, id, answer: { ending: { type: ending }, source } },
    });
    void closeTerminal();
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

  /**
   * Whether a key landed in a preview panel, which is a scroller before it is
   * anything else. By its own mark rather than its class, for the reason
   * `inField` gives.
   */
  const inPreview = (event: KeyboardEvent): boolean =>
    event.target instanceof HTMLElement && event.target.closest('[data-preview]') !== null;

  /** The keys a scroller answers itself, which are the only ones the dock yields. */
  const SCROLL_KEYS = new Set(['ArrowUp', 'ArrowDown', 'PageUp', 'PageDown', 'Home', 'End', ' ']);

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
    // The preview owns the keys that scroll it and nothing else: a character
    // typed there still starts the reader's own answer, and Enter still submits.
    // The tab stop is unconditional, so these keys leave the dock even over a
    // preview too short to scroll; a conditional one would need a live overflow
    // read, and jsdom cannot measure one to test it.
    if (inPreview(event) && SCROLL_KEYS.has(event.key)) return;
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
      // A link inside a preview is the link's: tabbed onto it, Enter must
      // follow the link rather than answer the row the dock has marked.
      if (event.target instanceof Element && event.target.closest('a') !== null) return;
      event.preventDefault();
      submit(false);
      return;
    }
    if (event.key === 'Escape') {
      event.preventDefault();
      // **The open field owns Escape before the card does.** One Tab from the
      // field lands on a verb, and a decline from there would take the words
      // and the whole draft; closing the field is the reader's way back.
      if (feedbackOpen) {
        closeFeedback();
        return;
      }
      if (vaultNoteOpen) {
        closeVaultNote();
        return;
      }
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
      {#if dictation && dictate !== null}
        <DictateControl
          {dictate}
          onpress={() => {
            // **Pressing the mic is being in the row.** It counts as use
            // whether or not the caret follows it, so the caret comes back to
            // the words row when the take's card leaves - which a touch screen
            // cannot reach any other way.
            visited = true;
            onmic('dock');
          }}
        />
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

  <!-- **A preview is a document, so it is one region.** Both the marked
       option's prose and a held Slack post are markdown the reader is being
       asked to approve, and both are the same panel as a result: the sheet
       bounds it and gives it its own scroll, and this is the one place that
       panel is drawn. **A tab stop, because it is a document**: the keys that
       read the rest of it have to reach it. It is not a widget, so it takes
       no widget role for being focusable. -->
  <!-- The one fold control, worn by every kind's head: the body is one slot,
       and the bar it folds to is the way back in. -->
  {#snippet foldButton()}
    <button
      class="chevb"
      type="button"
      aria-label="fold to its bar"
      onclick={() => {
        collapsed = true;
      }}
    >
      <Icon name="down" class="ic" />
    </button>
  {/snippet}

  {#snippet valueBlock(label: string, value: unknown)}
    <!-- The value can be taller than the card: a tab stop and a region label
         make it reachable and scrollable from the keyboard, the same contract
         an option's preview keeps. -->
    <!-- svelte-ignore a11y_no_noninteractive_tabindex -->
    <pre class="wv" tabindex="0" role="region" aria-label={label}>{showValue(value)}</pre>
  {/snippet}

  {#snippet previewRegion(label: string, text: string, source: boolean)}
    <!-- svelte-ignore a11y_no_noninteractive_tabindex -->
    <div class="preview" tabindex="0" role="region" aria-label={label} data-preview>
      <!-- **An option's preview is line-structured ASCII**, so it is drawn as
           its own source: a line beginning `+`, `-` or `#` is that line and
           not a list item or a heading (#1634). A held draft is markdown and
           renders as markdown. -->
      {#if source}
        <pre class="src">{text}</pre>
      {:else}
        <Prose {text} />
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
      <span class="qm"><Icon name={barMark} /></span>
      <span class="t">{barText}</span>
      {#if depth > 1}<span class="cnt">+{depth - 1}</span>{/if}
      <Icon name="fold-up" class="chevc" />
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
        {@render foldButton()}
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
          {@render foldButton()}
        </span>
      </div>
      <div class="desc">{ask.request.header}</div>
    {:else if ask.kind === 'slack_draft'}
      <div class="d-head">
        <span
          class="dest"
          class:tags-below={tagsBelow}
          class:pill-stacked={pillStacked}
          bind:this={destEl}
        >
          <span class="pin">
            <span class="ws" title={slackDest.workspace}>{slackDest.workspace}</span>
            <span class="sp" aria-hidden="true">·</span>
            <span class="ch" title={slackDest.conversation}>{slackDest.conversation}</span>
          </span>
          {#if slackDest.threadTs !== null}
            <span class="thr">
              <span class="lbl">{slackDest.noun}:</span>
              {#if slackDest.thread === null}
                <span class="ex">a {slackDest.noun}</span>
              {:else}
                {@const thread = slackDest.thread}
                <span class="who">@{thread.author}</span>
                {#if thread.excerpt !== ''}
                  <span class="ex" title={thread.excerpt}>"{thread.excerpt}"</span>
                {/if}
              {/if}
            </span>
          {/if}
          {#if slackDest.tagged.length > 0}
            {@const tagged = slackDest.tagged}
            <!-- The separator is an item of its own so a tag list that wraps
                 to its own row does not start with a bullet: it stays at the
                 end of the row above. -->
            <span class="tsep" aria-hidden="true">·</span>
            <span class="tgs">
              <span class="lbl">tags:</span>
              {#each tagged as person, at (person.id)}
                <span class="nm"
                  >{person.name === null ? person.id : `@${person.name}`}{at < tagged.length - 1
                    ? ','
                    : ''}</span
                >
              {/each}
            </span>
          {/if}
        </span>
        <span class="q mono dest-verb">{slackVerbs.head}</span>
        {@render foldButton()}
      </div>
      <!-- The words the draft would send, with its mentions drawn as the names
         the core resolved: this is what would go out and what the reader
         approves, not a summary of it. -->
      <!-- The draft reads as an option's preview does, through the same
         markdown renderer and the same panel: Slack text IS markdown, the
         question kinds are already in hand, and one kind of block for both
         keeps the dock one system rather than two. -->
      {@render previewRegion(`the ${ask.request.operation} held for approval`, slackBody, false)}
    {:else if ask.kind === 'document_hand_off'}
      <div class="d-head">
        <span class="tag warn">&#9888; needs you</span>
        <span class="q mono">document hand-off</span>
        {@render foldButton()}
      </div>
      <!-- The resolved path verbatim: it is the document's own address, and a
         basename would be the client guessing at which file was meant. -->
      <div class="d-q">{ask.request.path}</div>
      <div class="desc">
        Open reads it in the viewer{ask.request.reviewable
          ? ', where the session asked for comments'
          : ''}; the session waits until you finish. Not now tells it you are not reading this one.
      </div>
    {:else if ask.kind === 'settings_write'}
      <div class="d-head">
        <span class="tag warn">&#9888; needs you</span>
        <span class="q mono">config write</span>
        {@render foldButton()}
      </div>
      <!-- Every key and both values verbatim: approving a summary of an edit
         nobody drew would be approving something the reader never saw. A
         section's mutually required keys ride one card because they are one
         save. -->
      {#each ask.request.edits as edit (edit.key)}
        <div class="d-q mono">{edit.key}</div>
        {#if edit.current === null}
          <div class="desc">currently unset</div>
        {:else}
          <div class="desc">currently</div>
          {@render valueBlock('the value this key holds now', edit.current)}
        {/if}
        {#if edit.unset}
          <div class="desc">cleared to its default</div>
        {:else}
          <div class="desc">would become</div>
          {@render valueBlock('the value this write would leave', edit.proposed)}
        {/if}
      {/each}
    {:else if ask.kind === 'terminal_hand_off'}
      <div class="d-head">
        <span class="tag warn">&#9888; needs you</span>
        <span class="q mono">{handOffTitle}</span>
        {@render foldButton()}
      </div>
      <!-- The reason verbatim: it is what the session needs, in its own words,
         and a summary of it would be the client guessing at the act. -->
      <div class="d-q">{ask.request.reason}</div>
      <div class="desc">
        {raiseFailed ??
          "Open terminal raises the seat's terminal, exactly as it stands; take control there to type. The session waits, with no timeout, for your answer, and cannot read or write the terminal while it does."}
      </div>
    {:else if ask.kind === 'vault'}
      <div class="d-head">
        <span class="tag warn">&#9888; needs you</span>
        <span class="q mono">vault &middot; {ask.request.tool}</span>
        {@render foldButton()}
      </div>
      <!-- The summary and the facts below are the whole draft, and the draft
           carries no secret value: the card draws what the call would do,
           never what it would read. -->
      <div class="d-q">{ask.request.summary}</div>
      {#if ask.request.itemName !== null}
        <div class="desc">item: {ask.request.itemName}</div>
      {/if}
      {#if ask.request.itemId !== null}
        <div class="desc mono">id: {ask.request.itemId}</div>
      {/if}
      {#if ask.request.field !== null}
        <div class="desc">field: {ask.request.field}</div>
      {/if}
      {#if ask.request.maskedDiff !== null}
        <div class="desc">the change, with every secret value masked</div>
        {@render valueBlock('the change with every secret value masked', ask.request.maskedDiff)}
      {/if}
      {#if ask.request.attachment !== null}
        {@const attachment = ask.request.attachment}
        <div class="desc">
          attachment: {attachment.name} ({attachment.size} bytes){attachment.replaces !== null
            ? ` · replaces '${attachment.replaces}'`
            : ''}
        </div>
      {/if}
      {#if ask.request.needsUnlock}
        <div class="unlock">
          <Field
            editor="dock"
            class="fld"
            element="input"
            type="password"
            bind:value={masterPassword}
            placeholder="master password"
            aria={{ label: 'the vault master password' }}
            onkeydown={(event: KeyboardEvent) => {
              onUnlockKey(event);
            }}
          />
          <Field
            editor="dock"
            class="fld"
            element="input"
            bind:value={twoFactor}
            placeholder="2FA code (optional)"
            autocomplete="one-time-code"
            inputmode="numeric"
            aria={{ label: 'the two-factor code' }}
            onkeydown={(event: KeyboardEvent) => {
              onUnlockKey(event);
            }}
          />
        </div>
      {/if}
    {:else}
      <div class="d-head">
        <span class="tag warn">&#9888; needs you</span>
        <span class="q mono">{handOffTitle}</span>
        {@render foldButton()}
      </div>
      <!-- The reason verbatim: it is what the session needs, in its own words,
         and a summary of it would be the client guessing at the act. -->
      <div class="d-q">{ask.request.reason}</div>
      {#if carried !== null && carried.trim() !== ''}
        <div class="desc">
          What you said at the bar is in the chat's draft: {carried}
        </div>
      {/if}
      <div class="desc">
        {raiseFailed ??
          'Open browser shows the page the session prepared, exactly as it stands. The session waits, with no timeout, for your answer.'}
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
        {@render previewRegion("the marked option's preview", markedRow.preview, true)}
      {/if}
    {/if}

    {#if question}
      {@render wordsRow()}
    {/if}

    {#if ask.kind === 'slack_draft'}
      <div class="acts">
        <button class="btn p" onclick={() => post('approve')}>{slackVerbs.approve}</button>
        <button class="btn" bind:this={sendBackButton} onclick={openFeedback}
          >Send back with feedback</button
        >
        <button class="btn d" onclick={() => post('decline')}>{slackVerbs.decline}</button>
      </div>
      {#if feedbackOpen}
        <!-- The send-back's words, on the card the draft is drawn on: the
             reader says what is wrong where they read what would go out. -->
        <div class="sendback">
          {#if take !== null && tookFrom === 'feedback'}
            <!-- The take's card draws where the words will land, so a reader
                 who pressed the send-back mic sees their recording where the
                 words are going. -->
            <TakeCard {take} phase={take.phase} {wire} oncancel={onabandon} {silent} spread />
          {:else}
            <Field
              editor="feedback"
              class="notes"
              bind:value={feedback}
              rows={2}
              placeholder="What should change?"
              aria={{ label: 'feedback for the agent' }}
              onkeydown={(event: KeyboardEvent) => {
                onFeedbackKey(event);
              }}
              field={(el: HTMLElement | null) => {
                feedbackEl = el;
              }}
            />
            {#if dictation && dictate !== null}
              <DictateControl
                {dictate}
                onpress={() => {
                  onmic('feedback');
                }}
              />
            {/if}
          {/if}
          <button class="btn" disabled={feedback.trim() === ''} onclick={sendBack}>Send back</button
          >
        </div>
      {/if}
      {#if take !== null && !(tookFrom === 'feedback' && feedbackOpen)}
        <!-- A take still running behind a held draft: this branch draws no words
           row, so without this line a prompt would swallow a recording that is
           still going. The send-back box draws its own card while it is open,
           so the line stands down there. -->
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
           note line and back-to-chat live on the takeover's own bar.
           Before it opens, the card carries Open, Done and Not now - the
           session waits with no timeout, so a way out must be reachable. -->
        {#if surface.phase === 'live' && (ask.request.tab === null || surface.tab === ask.request.tab)}
          <span class="fo">the browser is open over the window</span>
        {:else}
          <!-- **The card's three, as Ved asked (2026-10-19):** Open browser,
               Done and Not now before the takeover; inside it the bar adds
               the note line and back to chat. -->
          <button class="btn p" onclick={open}>Open browser</button>
          <button class="btn" onclick={() => handOff('done', 'dock')}>Done</button>
          <button class="btn d" onclick={() => handOff('not_now', 'dock')}>Not now</button>
        {/if}
      </div>
    {/if}

    {#if ask.kind === 'terminal_hand_off'}
      <div class="acts">
        <!-- The surface carries the person's work once it is up; before then
             the card carries Open terminal, Done and Not now, because the
             session waits with no timeout. -->
        {#if terminalSurface.phase === 'live'}
          <span class="fo">the terminal is open over the window</span>
        {:else}
          <button class="btn p" onclick={openTerminalHandOff}>Open terminal</button>
          <button class="btn" onclick={() => terminalHandOff('done', 'dock')}>Done</button>
          <button class="btn d" onclick={() => terminalHandOff('not_now', 'dock')}>Not now</button>
        {/if}
        {#if ask.request.secret}
          <span class="fo">typing is hidden: the program is reading a secret</span>
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

    {#if ask.kind === 'settings_write'}
      <div class="acts">
        <!-- The write goes out or it does not: those two verbs are buttons
           under the values they would change. -->
        <button class="btn p" onclick={() => write(true)}>Write it</button>
        <button class="btn d" onclick={() => write(false)}>Don't write</button>
      </div>
    {/if}

    {#if ask.kind === 'vault'}
      <div class="acts">
        <!-- The call runs or it does not, and the send-back is the middle
           door. A locked card's Approve waits for the master password, since
           the approval is what performs the unlock. -->
        <button
          class="btn p"
          disabled={ask.request.needsUnlock && masterPassword === ''}
          onclick={() => answerVault('approve')}>Approve</button
        >
        <button class="btn" bind:this={vaultSendBackButton} onclick={openVaultNote}
          >Send back with a note</button
        >
        <button class="btn d" onclick={() => answerVault('decline')}>Decline</button>
      </div>
      {#if vaultNoteOpen}
        <!-- The send-back's words, on the card the call is drawn on: the
           reader says what should change where they read what would run. -->
        <SendBack
          bind:value={vaultNote}
          label="the note for the agent"
          onkeydown={(event: KeyboardEvent) => {
            onVaultNoteKey(event);
          }}
          field={(el: HTMLElement | null) => {
            vaultNoteEl = el;
          }}
          onsend={() => answerVault('send_back')}
        />
      {/if}
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
        {:else if ask.kind === 'slack_draft'}
          <!-- **The order is the order that works.** A held draft arrives with
             the keyboard on the dock root, where the arrows are the dock's own
             and move nothing; they scroll only once the keyboard is in the
             region, so the Tab leads. -->
          <span
            ><kbd>Tab</kbd> into the card, then <kbd>&uarr;</kbd><kbd>&darr;</kbd> scroll it</span
          >
          <span><kbd>Tab</kbd> again for the card's actions</span>
          <span><kbd>Enter</kbd> activates</span>
        {:else if ask.kind === 'settings_write'}
          <span><kbd>Tab</kbd> between the fold, Write it and Don't write</span>
          <span><kbd>Enter</kbd> activates</span>
        {:else if ask.kind === 'vault'}
          <span><kbd>Tab</kbd> between the fields and the card's verbs</span>
          <span><kbd>Enter</kbd> activates</span>
        {:else}
          <span><kbd>Tab</kbd> between the actions</span>
          <span><kbd>Enter</kbd> activates</span>
        {/if}
        {#if take !== null}
          <span><kbd>Esc</kbd> cancels the take, then rejects</span>
        {:else if ask.kind === 'vault'}
          <span><kbd>Esc</kbd> declines</span>
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
