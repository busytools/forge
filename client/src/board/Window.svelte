<script lang="ts">
  import { flip } from 'svelte/animate';
  import { fade, fly } from 'svelte/transition';
  import { hrefForSlot } from '../routes';
  import type { SessionSlot } from '../wire/types';
  import Icon from '../components/Icon.svelte';
  import BoardField from './BoardField.svelte';
  import Picker from './Picker.svelte';
  import { digestLines, sampleClaim } from './digest';
  import type { BoardVoice } from './voice';
  import { DISPOSITIONS } from './view';
  import type { NeedCard, WindowView } from './window';
  import { clockOf } from './window';

  /**
   * The window: the board drawn for a person, exceptions first.
   *
   * Four regions in the order a reader needs them - what needs them, what
   * looks wrong, who is doing what, and what is next. Every fact is the
   * server's own: this component orders and words, it never derives.
   *
   * Everything the reader presses goes out through `onact`, the page's own
   * dispatch, and a row's id opens the drawer through `onopen`.
   */
  let {
    view,
    onact = null,
    onopen = null,
    voice = null,
  }: {
    view: WindowView;
    onact?: ((command: Record<string, Record<string, unknown>>) => void) | null;
    onopen?: ((id: string) => void) | null;
    /** The page's own take, for the card's word boxes; `null` without one. */
    voice?: BoardVoice | null;
  } = $props();

  /** The note a decision card sends with its choice, per row. */
  const notes: Record<string, string> = $state({});
  /** The words a look's send-back carries, per row. */
  const words: Record<string, string> = $state({});
  /** Whether a card's note box is open: a card reads as an ask, not a form. */
  const noteOpen: Record<string, boolean> = $state({});
  /** Whether a card's evidence is expanded: one summary line until asked. */
  const evidenceOpen: Record<string, boolean> = $state({});
  /** Which digest line is open on its items, by the line's own key. */
  const digestOpen: Record<string, boolean> = $state({});
  /** The deferral the person chose, per row, so the control names it. */
  const snoozeChoice: Record<string, string> = $state({});

  /** How much of the evidence a collapsed card shows before it is expanded. */
  const SUMMARY_CHARS = 90;

  /** The two deferrals the snooze control offers, one menu, one press each. */
  const SNOOZE_OPTIONS = [
    { value: '1h', label: '1h' },
    { value: 'activity', label: 'until it moves' },
  ];

  /** The move's own duration: a reader who asked for less gets none. */
  function motion(): number {
    return window.matchMedia('(prefers-reduced-motion: reduce)').matches ? 0 : 180;
  }

  /** The value a card's door returns: the kind decides which one. */
  function answer(
    card: NeedCard,
    value: { choice?: string; text?: string; disposition?: string },
  ): void {
    const command: Record<string, unknown> = { project: view.project, id: card.id, ...value };
    // A teaching's answer IS the lesson: the policy note its class is
    // settled by.
    if (card.kind === 'teaching') command['lesson'] = value.text ?? '';
    // A note rides beside a pressed option or disposition. Where the box IS
    // the answer (a clarification's words, a teaching's lesson), sending it
    // again as a note would say the same thing twice.
    if (value.text === undefined) {
      const note = notes[card.id]?.trim();
      if (note !== undefined && note !== '') command['words'] = note;
    }
    act({ task_answer: command });
  }

  /** The person marked an ask seen: theirs to press, never an agent's. */
  function acknowledge(card: NeedCard): void {
    act({ task_ack: { project: view.project, id: card.id } });
  }

  /** The person deferred an ask until a time, or until the row moves. */
  function snooze(card: NeedCard, until: string): void {
    snoozeChoice[card.id] = until;
    act({ task_snooze: { project: view.project, id: card.id, until } });
  }

  /** What the snooze control reads: the state it wrote, else the press. */
  function snoozeShown(card: NeedCard): string {
    if (!card.snoozed) return 'snooze';
    return snoozeChoice[card.id] ?? 'snoozed';
  }

  /**
   * The all-clear's whole sentence: a claim, and the evidence behind it.
   * One interpolation, so the sentence reads as one line however the markup
   * wraps.
   */
  function claimLine(view: WindowView): string {
    const attention = view.attention;
    if (attention === null) return 'nothing is waiting on you';
    const at = clockOf(attention.checkedAt ?? Math.floor(Date.now() / 1_000));
    const agents = `${attention.agents} ${attention.agents === 1 ? 'agent' : 'agents'} ran`;
    return `nothing needs you - checked ${at} · ${agents} · ${sampleClaim(attention)}`;
  }

  function summaryOf(text: string): string {
    return text.length > SUMMARY_CHARS ? `${text.slice(0, SUMMARY_CHARS).trimEnd()}...` : text;
  }

  function act(command: Record<string, Record<string, unknown>>): void {
    onact?.(command);
  }

  /** The seat's page, where the discuss door opens the ask in the chat. */
  function discussHref(seat: SessionSlot): string {
    return hrefForSlot(seat);
  }

  /** The chip an evidence state draws; `null` for a row carrying none. */
  function evidenceChip(state: 'verified' | 'unchecked' | 'failed' | null): {
    label: string;
    tone: string;
  } | null {
    if (state === 'verified') return { label: 'evidence verified', tone: 'blue' };
    if (state === 'failed') return { label: 'evidence refused', tone: 'bad' };
    if (state === 'unchecked') return { label: 'evidence unchecked', tone: 'warn' };
    return null;
  }
</script>

<section class="w-window" aria-label="the board window">
  {#if view.attention !== null}
    <section class="w-digest" aria-label="since your last look">
      <p class="w-digest-title">since your last look</p>
      <!-- **One line per count, each opening onto the rows it counts.** A
           line with nothing behind it is still drawn - a fact about the
           window, not an absence - and an expired ask keeps its record
           here, drawn as expired. -->
      <ul class="w-digest-lines">
        {#each digestLines(view.attention) as line (line.key)}
          {@const open = digestOpen[line.key] === true}
          <li class="w-digest-row">
            {#if line.items.length > 0}
              <button
                type="button"
                class="w-digest-line"
                aria-expanded={open}
                aria-label="{line.label}: {line.countLabel}"
                onclick={() => (digestOpen[line.key] = !open)}
              >
                <span class="w-digest-label">{line.label}</span>
                <span class="w-n">{line.countLabel}</span>
                <span class="w-digest-caret" aria-hidden="true">{open ? '-' : '+'}</span>
              </button>
            {:else}
              <span class="w-digest-line w-digest-static">
                <span class="w-digest-label">{line.label}</span>
                <span class="w-n">{line.countLabel}</span>
              </span>
            {/if}
            {#if open}
              <ul class="w-list w-sample" aria-label="the {line.label} items">
                {#each line.items as item, index (item.id ?? `${line.key}:${index}`)}
                  <li class="w-exc">
                    {#if item.id !== null}
                      <button
                        type="button"
                        class="w-link"
                        onclick={() => onopen?.(item.id as string)}>{item.subject}</button
                      >
                    {:else}
                      <span class="w-strong">{item.subject}</span>
                    {/if}
                    <span class="w-why">{item.word}</span>
                    <span class="w-why">{clockOf(item.at)}</span>
                  </li>
                {/each}
              </ul>
            {/if}
          </li>
        {/each}
      </ul>
      {#if view.attention.refused !== null && view.attention.refused !== undefined}
        <!-- **A count the store could not read is not a zero.** The strip
             says which half is missing rather than drawing an absence. -->
        <p class="w-digest-warn">{view.attention.refused}</p>
      {/if}
    </section>
  {/if}
  <section class="w-part" aria-labelledby="w-needs-h">
    <h2 id="w-needs-h" class="w-h">
      needs you {#if view.needs.length > 0}<span class="w-n">{view.needs.length}</span>{/if}
    </h2>
    {#if view.needs.length === 0}
      <!-- **The empty state is a claim with its evidence**, not an absence:
           when it was checked, how many agents acted, what was sampled, and
           what changed since the person last looked. -->
      {#if view.attention !== null}
        <p class="w-claim">{claimLine(view)}</p>
      {:else}
        <p class="w-quiet">nothing is waiting on you</p>
      {/if}
    {:else}
      <div class="w-needs">
        {#each view.needs as card (card.id)}
          {@const chip = evidenceChip(card.evidenceState)}
          {@const expanded = evidenceOpen[card.id] === true}
          {@const note = noteOpen[card.id] === true}
          <article
            class="w-need"
            class:snoozed={card.snoozed}
            class:seen={card.acknowledged}
            data-id={card.id}
            animate:flip={{ duration: motion() }}
            in:fly={{ y: 8, duration: motion() }}
            out:fade={{ duration: motion() }}
          >
            <div class="w-meta">
              <span class="w-kind"
                >{#if card.kind === 'look'}<Icon name="check" /> a look{:else}{card.kind}{/if}</span
              >
              <span class="w-ctx"
                >{card.subject}{#if card.owner !== null}
                  &middot; {card.owner}{/if}</span
              >
              {#if card.class !== null}<span class="w-chip" title="the class a teaching settles"
                  >{card.class}</span
                >{/if}
              {#if card.acknowledged}
                <span
                  class="w-chip"
                  class:warn={card.forgotten}
                  title={card.forgotten
                    ? "acknowledged and still unanswered past the drive's reminder"
                    : null}>{card.forgotten ? 'seen, forgotten' : 'seen'}</span
                >
              {/if}
              {#if card.snoozed}<span class="w-chip">snoozed</span>{/if}
              {#if card.dueAt !== null}
                <span class="w-chip" title="the default that runs if it is ignored"
                  >{card.dueDefault ?? 'due'} at {clockOf(card.dueAt)}</span
                >
              {/if}
              <span class="w-age">waiting {card.age}</span>
            </div>
            <h3 class="w-q">{card.question}</h3>
            {#if card.artifact !== null}
              <a class="w-door" href={card.artifact.href} target="_blank" rel="noreferrer"
                >look at <span class="nm">{card.artifact.label}</span></a
              >
            {/if}
            {#if chip !== null || card.evidence !== null}
              <div class="w-ev" class:open={expanded}>
                {#if chip !== null}<span class="b-chip {chip.tone}">{chip.label}</span>{/if}
                {#if card.evidence !== null}
                  <!-- The full line rides the title whatever the card's
                       width does to the drawn one, and `more` is always
                       drawn: a character count cannot know the card's
                       width, and a touch reader has no hover for the
                       title. -->
                  <span class="w-ev-text" title={card.evidence}
                    >{expanded ? card.evidence : summaryOf(card.evidence)}</span
                  >
                  <button
                    type="button"
                    class="w-more"
                    aria-expanded={expanded}
                    onclick={() => (evidenceOpen[card.id] = !expanded)}
                    >{expanded ? 'less' : 'more'}</button
                  >
                {/if}
                <!-- The excerpt a lead's quote folded away keeps its own
                     line under it, once the reader asks for the rest. -->
                {#if expanded && card.excerpt !== null}
                  <span class="w-ev-text w-ev-excerpt">{card.excerpt}</span>
                {/if}
              </div>
            {/if}
            <!-- One action bar: filled primaries, an outlined secondary, and
                 the note as a ghost button. The row's meta doors are the
                 footer's, never this bar's. -->
            <div class="w-actions">
              {#if card.kind === 'look'}
                {#if card.approveBlocked !== null}
                  <p class="w-hold">{card.approveBlocked}</p>
                {:else}
                  <button
                    type="button"
                    class="b-btn b-ok w-primary"
                    onclick={() =>
                      act({
                        task_verdict: { project: view.project, id: card.id, approve: true },
                      })}>approve</button
                  >
                {/if}
                <button
                  type="button"
                  class="b-btn b-no w-secondary"
                  onclick={() =>
                    act({
                      task_verdict: {
                        project: view.project,
                        id: card.id,
                        approve: false,
                        words: words[card.id] ?? '',
                      },
                    })}>send back</button
                >
              {:else if card.kind === 'decision'}
                {#if card.options.length > 0}
                  <div class="w-options" role="group" aria-label="answer with one of">
                    {#each card.options as option (option)}
                      <button
                        type="button"
                        class="w-primary"
                        onclick={() => answer(card, { choice: option })}>{option}</button
                      >
                    {/each}
                  </div>
                {/if}
              {:else if card.kind === 'exception'}
                <div class="w-options" role="group" aria-label="dispose of it">
                  {#each DISPOSITIONS as disposition (disposition)}
                    <button
                      type="button"
                      class="w-primary"
                      onclick={() => answer(card, { disposition })}>{disposition}</button
                    >
                  {/each}
                </div>
              {/if}
              {#if card.kind === 'clarification' || card.kind === 'teaching'}
                {#if note}
                  <BoardField
                    id="{card.id}:note"
                    value={notes[card.id] ?? ''}
                    oninput={(text: string) => (notes[card.id] = text)}
                    {voice}
                    placeholder={card.kind === 'teaching' ? 'the lesson' : 'your answer'}
                    label={card.kind === 'teaching'
                      ? 'the lesson that settles the class'
                      : 'your answer'}
                  />
                  <button
                    type="button"
                    class="w-primary"
                    onclick={() => answer(card, { text: notes[card.id] ?? '' })}
                    >{card.kind === 'teaching' ? 'settle the class' : 'answer'}</button
                  >
                {:else}
                  <button
                    type="button"
                    class="w-ghost"
                    aria-expanded={note}
                    onclick={() => (noteOpen[card.id] = true)}
                    >{card.kind === 'teaching'
                      ? 'write the lesson'
                      : 'answer in your words'}</button
                  >
                {/if}
              {:else if card.kind === 'look'}
                {#if note}
                  <BoardField
                    id="{card.id}:note"
                    value={words[card.id] ?? ''}
                    oninput={(text: string) => (words[card.id] = text)}
                    {voice}
                    placeholder="what to change"
                    label="what to change (sent back with it)"
                  />
                {:else}
                  <button
                    type="button"
                    class="w-ghost"
                    aria-expanded={note}
                    onclick={() => (noteOpen[card.id] = true)}>add a note</button
                  >
                {/if}
              {:else if card.kind === 'decision' && card.options.length === 0}
                <!-- **A decision with no enumerated options is still a
                     choice**: the words the person types ARE the choice, sent
                     as one, never as free text the core would refuse. -->
                {#if note}
                  <BoardField
                    id="{card.id}:note"
                    value={notes[card.id] ?? ''}
                    oninput={(text: string) => (notes[card.id] = text)}
                    {voice}
                    placeholder="your choice"
                    label="the choice you are naming"
                  />
                  <button
                    type="button"
                    class="w-primary"
                    disabled={(notes[card.id] ?? '').trim() === ''}
                    onclick={() => answer(card, { choice: (notes[card.id] ?? '').trim() })}
                    >answer</button
                  >
                {:else}
                  <button
                    type="button"
                    class="w-ghost"
                    aria-expanded={note}
                    onclick={() => (noteOpen[card.id] = true)}>answer in your own words</button
                  >
                {/if}
              {:else if note}
                <BoardField
                  id="{card.id}:note"
                  value={notes[card.id] ?? ''}
                  oninput={(text: string) => (notes[card.id] = text)}
                  {voice}
                  placeholder="note (optional)"
                  label="the note sent with the answer"
                />
              {:else}
                <button
                  type="button"
                  class="w-ghost"
                  aria-expanded={note}
                  onclick={() => (noteOpen[card.id] = true)}>add a note</button
                >
              {/if}
            </div>
            <!-- The person's own acts, one row of small quiet controls: each
                 is a shaped pill at the card's radius, content-sized, and the
                 row folds when the card is narrow rather than squeezing a
                 label into two lines. `mark seen` is the ghost of the pair,
                 and both it and snooze draw the state they wrote. -->
            <div class="w-foot">
              {#if card.seat !== null}
                <a class="w-foot-link" href={discussHref(card.seat)}>discuss</a>
              {/if}
              <button type="button" class="w-foot-link" onclick={() => onopen?.(card.id)}
                >open the row</button
              >
              {#if card.acknowledged}
                <span class="w-foot-link w-seen"><Icon name="check" /> seen</span>
              {:else}
                <button
                  type="button"
                  class="w-foot-link w-foot-ghost"
                  onclick={() => acknowledge(card)}>mark seen</button
                >
              {/if}
              <Picker
                label="snooze this ask"
                value={snoozeShown(card)}
                options={SNOOZE_OPTIONS}
                onpick={(until: string) => snooze(card, until)}
                class={card.snoozed ? 'w-foot-pill w-foot-on' : 'w-foot-pill'}
              />
            </div>
          </article>
        {/each}
      </div>
    {/if}
  </section>

  <section class="w-part" aria-labelledby="w-exceptions-h">
    <h2 id="w-exceptions-h" class="w-h">
      exceptions {#if view.exceptions.length > 0}<span class="w-n">{view.exceptions.length}</span
        >{/if}
    </h2>
    {#if view.exceptions.length === 0}
      <p class="w-quiet">nothing contradicts the board</p>
    {:else}
      <ul class="w-list">
        {#each view.exceptions as item (item.id ?? `worker:${item.subject}`)}
          <li class="w-exc">
            {#if item.id !== null}
              <button type="button" class="w-link" onclick={() => onopen?.(item.id as string)}
                >{item.subject}</button
              >
            {:else}
              <span class="w-strong">{item.subject}</span>
            {/if}
            <span class="w-why">{item.line}</span>
          </li>
        {/each}
      </ul>
    {/if}
  </section>

  <section class="w-part" aria-labelledby="w-live-h">
    <h2 id="w-live-h" class="w-h">
      live {#if view.live.length > 0}<span class="w-n">{view.live.length}</span>{/if}
    </h2>
    {#if view.live.length === 0}
      <p class="w-quiet">no seats are open on this project</p>
    {:else}
      <ul class="w-list">
        {#each view.live as line (line.label)}
          <li class="w-live">
            <a class="w-who" href={discussHref(line.seat)}>{line.label}</a>
            {#if line.doing === null}
              <span class="w-why">holds nothing</span>
            {:else}
              <span class="w-strong">{line.doing}</span>
              {#if line.since !== null}
                <span class="w-why">since {clockOf(line.since)}</span>
              {:else}
                <span class="w-why">since unknown</span>
              {/if}
              {#if line.artifact !== null}
                <span class="w-why">{line.artifact}</span>
              {:else}
                <span class="w-why">nothing touched yet</span>
              {/if}
              <span class="w-why">{line.next === null ? 'no estimate' : `next ${line.next}`}</span>
              {#if line.held > 1}
                <span class="w-why">and {line.held - 1} more held</span>
              {/if}
            {/if}
          </li>
        {/each}
      </ul>
    {/if}
  </section>

  <section class="w-part" aria-labelledby="w-next-h">
    <h2 id="w-next-h" class="w-h">
      next {#if view.next.length > 0}<span class="w-n">{view.next.length}</span>{/if}
    </h2>
    {#if view.next.length === 0}
      <p class="w-quiet">the queue is empty</p>
    {:else}
      <ol class="w-list w-next">
        {#each view.next as row (row.id)}
          <li>
            <button type="button" class="w-link" onclick={() => onopen?.(row.id)}
              >{row.subject}</button
            >
            {#if row.standing}
              <span class="w-why"
                >standing{row.measure === null ? '' : ` \u{b7} every ${row.measure}`}</span
              >
            {:else if row.measure !== null}
              <span class="w-why">{row.measure}</span>
            {/if}
          </li>
        {/each}
      </ol>
    {/if}
  </section>
</section>
