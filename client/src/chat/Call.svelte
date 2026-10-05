<script lang="ts">
  import { untrack } from 'svelte';

  import Chevron from '../components/Chevron.svelte';
  import Icon from '../components/Icon.svelte';
  import Code from './Code.svelte';
  import Decision from './Decision.svelte';
  import Call from './Call.svelte';
  import { iconOf } from './families';
  import { languageFor, type CallBody, type ToolLeaf } from './leaves';
  import { duration, tokens } from './numbers';
  import Prose from './Prose.svelte';
  import { renderInlineProse } from './prose';
  import { subagents } from './subagents.svelte';
  import { joinedLine, searchHits } from './text';
  import { dispatchFrames } from './timeline';

  /**
   * One call: what it was, whether it came back, and what it came back with.
   *
   * The group above keys this row by the row's own key, which for a call is
   * the id the wire gave it. A place in the list is not a key: a page landing
   * with its copy of the turn can put a block above this row, and every place
   * below it shifts - which remounts the row and closes what the reader had
   * open in it.
   *
   * `open` is what the row's own kind decides - a mutation's diff is drawn
   * without being asked, while it is small enough to draw - and the reader's
   * own toggling takes it from there.
   */
  let {
    call,
    open = false,
    k,
    messages = null,
  }: {
    call: ToolLeaf;
    open?: boolean;
    /**
     * The page's messages, across every turn, as the wire carried them.
     *
     * Read only by a dispatch row that is open: the instance's own frames
     * (its calls, their results, the prose between them) carry the parent id
     * that names the dispatch, and the row's timeline is read from them here
     * rather than from the card, whose tail caps at four. Every turn rather
     * than the row's own, because a backgrounded instance's frames arrive in
     * LATER turns than the dispatch that opened it.
     */
    messages?: readonly unknown[] | null;
    /**
     * The fold's own name for this row, which the row draws in `data-k`.
     *
     * **Required, and the wire id would not do**: an id-less `tool_use` leaves
     * it empty, and two such rows would carry one key. The leaves list hands
     * the fold's key down.
     *
     * **The column's anchor does not look this far down today**: its scan
     * takes the first row whose box crosses the viewport's top, and the unit
     * row enclosing this one always comes first in document order - so this
     * key is for a finer hold than the unit's, not the one in force, and
     * nothing is spent on it while the scan stops at the unit.
     */
    k: string;
  } = $props();

  /**
   * Whether the row is open, held HERE rather than drawn from the prop.
   *
   * The prop says where the row starts - a mutation's diff is open without
   * being asked, while it is small enough to draw - and the element owns the
   * state from then on: a row is re-rendered whenever the turn is, and an
   * `open` attribute written from a prop on every update closes a row the
   * reader has just opened.
   *
   * **A late opener is the one exception, and only its edge.** A decision's
   * block arrives with the result, after the row has mounted closed, so a
   * snapshot alone would draw the live and re-read paths differently; the row
   * opens on the false -> true edge of the prop and never again, so a reader
   * who closed it stays closed. An edit's diff is in the call at mount, so
   * that edge only ever fires for a block that arrived late.
   */
  let opened = $state(untrack(() => open));

  $effect(() => {
    if (open && !untrack(() => opened)) opened = true;
  });

  /** The tools whose body is a list of hits rather than prose or a command's output. */
  const SEARCHES = new Set(['Grep', 'Glob', 'LS']);

  /** The hits a search call came back with, or `null` when this is not one. */
  const hits = $derived(
    // A failed search carries no hits: its reason must draw through the
    // pieces, not be swallowed by a hits path that has nothing to list.
    SEARCHES.has(call.name) && !call.body.some((piece) => piece.kind === 'error')
      ? searchHits(call.body.map((piece) => (piece.kind === 'text' ? piece.text : '')).join('\n'))
      : null,
  );

  /** The language a read's body is drawn in, when the call named a file. */
  function asCode(piece: CallBody): string | null {
    return piece.kind === 'text' ? languageFor(call) : null;
  }

  /** A piece the patch box does not draw: a run of text, or a picture. */
  type RunPiece = Exclude<CallBody, { kind: 'diff' } | { kind: 'hunk' }>;

  /**
   * Which box a backgrounded call's notice goes at the end of: the last one
   * the call's result drew, which is where the drawing puts it.
   *
   * `-1` when the body drew no line at all, which is a result carrying only an
   * image - the notice is drawn in a box of its own there, because a line with
   * nowhere to sit is a line dropped.
   */
  const rest = $derived(
    call.body.filter((piece) => piece.kind !== 'diff' && piece.kind !== 'hunk'),
  );

  /**
   * The diff and hunk pieces, which draw inside ONE box.
   *
   * **Several hunks are several parts of one change**, and a box each made them
   * read as unrelated cards: two filled headers with nothing between them, on a
   * row whose title already names the file both of them are in. One box, and a
   * hairline carrying each hunk's range is the whole of what separates them.
   */
  const patches = $derived(
    call.body.filter((piece) => piece.kind === 'diff' || piece.kind === 'hunk'),
  );
  const tail = $derived(rest.map((piece) => piece.kind).lastIndexOf('text'));

  /**
   * Whether the change only adds, which is what a file the call created is.
   *
   * **Then there is no old side, and no old-number column to draw**: a fixed
   * column of blank numbers is the width of a gap on every line of a new file.
   * Read across every part of the change, not per hunk: a file is new or it is
   * not, and one of its hunks having an old side is what says it is not.
   */
  const added = $derived(
    patches.length > 0 &&
      patches.every((piece) =>
        piece.kind === 'hunk' ? piece.lines.every((line) => line.old === null) : piece.old === '',
      ),
  );

  /**
   * Whether the change is the call's own two sides, which have no position.
   *
   * A hunk the CLI wrote carries line numbers, and the columns for them; the
   * two sides an input carries have none, and two empty columns are the width
   * of a gap on every line of it - so the columns go with the numbers.
   */
  const bare = $derived(patches.length > 0 && patches.every((piece) => piece.kind === 'diff'));

  /**
   * What an image result draws under its picture: every piece the picture is
   * not.
   *
   * The `<img>` above IS the drawing of the result's own image block, so that
   * piece does not draw again; the rest - the path a screenshot was saved to,
   * the code that produced it - reached the page nowhere while the picture drew
   * from a branch of its own.
   */
  const aside = $derived(rest.filter((piece) => piece.kind !== 'image'));

  /**
   * The size of a mutation's change, as the one line under its diff, or `null`
   * for a call whose row draws no diff at all.
   *
   * Built here rather than in the markup: the line is one run of figures, and
   * the marks that follow it are the only parts that are elements.
   */
  const size = $derived(
    call.mutation === null || call.mutation.hunks === 0
      ? null
      : `${call.mutation.hunks} ${call.mutation.hunks === 1 ? 'hunk' : 'hunks'} \u{b7} +${call.mutation.added} \u{2212}${call.mutation.removed}`,
  );

  /**
   * The tone the kind's glyph takes once the call settles - green came back,
   * red failed - and nothing while it still runs: the loader is what says so
   * then.
   */
  const tone = $derived(
    call.status === 'completed'
      ? ' ok'
      : call.status === 'failed' || call.status === 'killed'
        ? ' err'
        : '',
  );

  /**
   * The sub-agent instance this call dispatched, when it is one.
   *
   * A dispatch row draws the INSTANCE's liveness rather than the call's own:
   * the launch-ack answers the call in a second while the agent runs on for
   * minutes, so the call's status alone would draw a finished row over work
   * that is still going.
   */
  const card = $derived(subagents.by(call.id));

  /** Whether the row reads as work in flight, which for a dispatch is the instance's own answer. */
  const running = $derived(
    card === undefined
      ? call.status !== 'completed' && call.status !== 'failed' && call.status !== 'killed'
      : card.running,
  );

  /**
   * Whether the row's text brightens, which is the narrower condition the
   * group's own rule uses: a call that has a frame in hand. A dispatch takes
   * the instance's answer instead, so it brightens for as long as the agent
   * actually works rather than for the second the ack takes.
   */
  const bright = $derived(card === undefined ? call.status === 'in_progress' : card.running);

  /** The kind glyph's tone, off the instance when this row carries one. */
  const shownTone = $derived(
    card === undefined ? tone : card.failed ? ' err' : card.running ? '' : ' ok',
  );

  /**
   * The figures a settled dispatch draws at the row's right: how many calls
   * it made, the tokens it spent, and how long it took. Tokens and the clock
   * come from the usage the CLI reported, so each draws only when it is
   * there; the call count always is.
   */
  const figures = $derived(
    card === undefined || card.running
      ? null
      : [
          `${card.calls} ${card.calls === 1 ? 'call' : 'calls'}`,
          card.usage === null ? null : `${tokens(card.usage.total_tokens)} tokens`,
          card.usage === null ? null : duration(card.usage.duration_ms),
        ]
          .filter((part) => part !== null)
          .join(' \u{b7} '),
  );

  /**
   * The dispatch's own frames, read only while the row is open: a shut row
   * pays nothing for a timeline nobody is looking at.
   */
  const sub = $derived(
    card !== undefined && opened ? dispatchFrames(messages ?? [], call.id) : null,
  );

  /** Whether this dispatch owns its RESULT text on the row: only a foreground
   * one does. A backgrounded instance's result is the launch ack, so that block
   * is not drawn - its task facts and the transcript path still are, because
   * the file's existence is a fact about the instance either way. */
  const grounded = $derived(card !== undefined && !card.backgrounded);

  /** The meta line's own pairs: what the CLI said about the task itself. */
  const meta = $derived(
    sub === null
      ? []
      : (
          [
            sub.model === null ? null : ['model', sub.model],
            sub.taskType === null ? null : ['task', sub.taskType],
            sub.taskId === null ? null : ['agent', sub.taskId],
            sub.depth === null ? null : ['depth', String(sub.depth)],
            sub.isolation === null ? null : ['isolation', sub.isolation],
          ] as ([string, string] | null)[]
        ).filter((part): part is [string, string] => part !== null),
  );
</script>

<details
  class="leaf"
  class:running={bright}
  bind:open={opened}
  data-k={`call-${k}`}
  data-sg={card?.dispatch_id}
>
  <summary>
    <Icon name={card === undefined ? iconOf(call.row) : 'subagents'} class={`gl${shownTone}`} />
    {#if running}
      <span class="st"><span class="ring"></span></span>
    {:else if card !== undefined && card.failed}
      <!-- A shape, not the glyph's tint alone: a failed instance and a clean
           one must not differ by colour only on a closed row. -->
      <Icon name="x" class="gl err" />
    {/if}
    <span class="tn">{call.title}</span>
    {#if card !== undefined && card.agent_type !== null}
      <span class="sg-ty">{card.agent_type}</span>
    {/if}
    {#if card !== undefined && card.backgrounded}
      <span class="sg-chip">background</span>
    {/if}
    {#if figures !== null}
      <span class="sg-fig">{figures}</span>
    {/if}
    <Chevron />
  </summary>

  {#if sub !== null}
    <!-- The instance's own expansion: the brief it was given, what it is
         doing now, and every call it has made with what each came back with.
         The frames carry the instance's work in full; the card's tail caps at
         four, and the record's usage covers the figures. -->
    <div class="body">
      {#if sub.brief !== null}
        <!-- The brief is markdown, as prompts are: rendered, not spooned out
             as raw text with its structure collapsed. -->
        <div class="sg-brief">
          <span class="sg-lb">brief</span>
          <Prose text={sub.brief} />
        </div>
      {/if}

      {#if sub.lines.length > 0}
        <div class="sg-tl">
          {#each sub.lines as line, at (at)}
            {#if line.kind === 'call'}
              <!-- The instance's calls are the session's own tool rows: the
                   same component, the same glyph and body, because a second
                   rendering of the same facts is a second thing to keep in
                   step. -->
              <Call call={line.leaf} k={line.leaf.id} />
              {#if line.leaf.status !== 'completed' && line.leaf.status !== 'failed' && line.leaf.status !== 'killed' && sub.beats.has(line.leaf.id)}
                <div class="sg-hb"><span class="ring"></span>heartbeat</div>
              {/if}
            {:else if line.kind === 'thought'}
              <!-- The instance's reasoning, drawn as the session draws a
                   thought: same glyph, same one-line lead, same markdown body
                   behind the open. -->
              <details class="leaf">
                <summary>
                  <Icon name="message-circle-more" class="gl" />
                  <!-- eslint-disable-next-line svelte/no-at-html-tags -->
                  <span class="tn">{@html renderInlineProse(joinedLine(line.text))}</span>
                  <Chevron />
                </summary>
                <div class="body"><Prose text={line.text} /></div>
              </details>
            {:else}
              <div class="sg-pr"><Prose text={line.text} /></div>
            {/if}
          {/each}
        </div>
      {/if}

      <!-- The task facts, for both carriers: only the transcript PATH is
           dropped for a backgrounded instance, whose own text is the launch
           ack and says nothing a reader acts on - the facts are the
           instance's either way. -->
      {#if meta.length > 0 || (grounded && sub.outputFile !== null)}
        <div class="sg-meta">
          {#each meta as [label, value], at (label)}{#if at > 0}
              &#183;
            {/if}<span>{label}</span>
            {value}{/each}{#if grounded && sub.outputFile !== null}<br /><span>output file</span>
            {sub.outputFile}{/if}
        </div>
      {/if}

      {#if grounded && call.body.length > 0}
        <!-- A dispatch's own result text - the launch ack, the hand-back, the
             notification's report - is prose, so it renders as markdown here
             rather than as a terminal box: the report is a list and its
             backticks are code, and spooning it out raw showed both. One
             wrapper for the row's own prose voice, so it never reads in the
             conversation's. -->
        <div class="sg-result">
          <span class="sg-lb">output</span>
          {@render pieces(rest, true)}
        </div>
      {/if}
    </div>
  {:else if call.image !== null}
    <!-- The picture the call read, drawn only while the row is open: decoding
         a screenshot is real work, and a column of closed rows must not pay
         it. The harness's own line about it rides under as the caption, and
         the result's text rides under that: the picture is one block of the
         result, not the whole of it. -->
    <div class="body">
      {#if opened}
        <div class="shot">
          <img src={`data:${call.image.mime};base64,${call.image.data}`} alt={call.title} />
          {#if call.imageNote !== null}
            <div class="note">{call.imageNote}</div>
          {/if}
        </div>
      {/if}
      {@render pieces(aside)}
    </div>
  {:else if call.skill !== null}
    <!-- A `Skill` call's own result is the CLI's launching line; the row opens
         onto the skill itself, which is what anyone opening it wants to read. -->
    <div class="body">
      <Prose text={call.skill} />
    </div>
  {:else if call.decision !== null}
    <!-- The result's own JSON is the same facts undressed; the block is how
         they read, and an unreadable result never reaches this branch. -->
    <div class="body">
      <Decision decision={call.decision} />
    </div>
  {:else if call.body.length > 0}
    <div class="body">
      {#if hits !== null}
        {#each hits as hit, at (at)}
          <!-- The location and the line are two elems: one run with a
               newline in it collapses here, and the hit then draws as a
               single line with its own location run into its text. -->
          <div class="searchhit">
            <div class="where">
              <span class="ln">{hit.line}:</span> <span class="fl">{hit.path}</span>
            </div>
            {#if hit.src !== ''}<div class="src">{hit.src}</div>{/if}
          </div>
        {/each}
      {:else}
        {#if patches.length > 0}
          <div class="dif" class:added class:bare>
            {#each patches as piece, at (at)}
              {#if piece.kind === 'diff'}
                <!-- No header naming the file: the row's own title is the path,
                     and it is the same path, so a line here would print it
                     twice. No number columns either: this is the call's own two
                     sides, which have no position to number (`bare`). -->
                {#each piece.old.split('\n') as line, n (`old-${n}`)}
                  {#if piece.old !== ''}
                    <div class="ln d">
                      <span class="n">{'\u{2212}'}</span><span class="l">{line}</span>
                    </div>
                  {/if}
                {/each}
                {#each piece.new.split('\n') as line, n (`new-${n}`)}
                  {#if piece.new !== ''}
                    <div class="ln a">
                      <span class="n">+</span><span class="l">{line}</span>
                    </div>
                  {/if}
                {/each}
              {:else}
                <!-- The CLI's own hunk, which is where the change sits and what
                     is around it. The header is the range it covers, and each
                     line is read by the mark the wire prefixes it with. -->
                <div class="h">{piece.header}</div>
                {#each piece.lines as line, n (`h-${n}`)}
                  <div
                    class="ln"
                    class:d={line.kind === 'del'}
                    class:a={line.kind === 'add'}
                    class:ctx={line.kind === 'ctx'}
                  >
                    <span class="on">{line.old ?? ''}</span>
                    <span class="nn">{line.new ?? ''}</span>
                    <span class="n"
                      >{line.kind === 'del' ? '\u{2212}' : line.kind === 'add' ? '+' : ''}</span
                    >
                    <span class="l">{line.text}</span>
                  </div>
                {/each}
              {/if}
            {/each}
          </div>
        {/if}
        {@render pieces(rest)}
        {#if tail === -1 && call.note !== null}
          <div class="term"><span class={call.note.tone ?? undefined}>{call.note.text}</span></div>
        {/if}
        {#if size !== null}
          <!-- The size always, then only the marks that are true: a reader acts
               on "every match" and on a file that moved, and the default of
               each is nothing to say.
               On ONE line, because the box is `white-space: pre-wrap`: a
               newline in this template is a newline on screen. -->
          <!-- prettier-ignore -->
          <div class="patchline"><span class="patchsize">{size}</span>{#if call.mutation?.all}<span class="patchmark">every match</span>{/if}{#if call.mutation?.outside}<span class="patchmark">changed outside this edit</span>{/if}</div>
        {/if}
      {/if}
    </div>
  {/if}
</details>

<!-- One statement of how a result's pieces draw, so the picture's branch and
     the body's branch cannot drift apart. -->
{#snippet pieces(parts: RunPiece[], asProse = false)}
  {#each parts as piece, at (at)}
    {#if piece.kind === 'image'}
      <div class="term">
        image{#if piece.mime}{' \u{b7} '}{piece.mime}{/if}{#if piece.uri}{' \u{b7} '}{piece.uri}{/if}
      </div>
    {:else if piece.kind === 'error'}
      <!-- A failed call's reason, in the caption chrome rather than a box of
           its own: what happened first, the detail under it. The CLI's
           envelope was read off in the fold, so only the words arrive here -
           and the command leads its own failure the way it leads a settled
           one, or the reason says nothing about what failed. -->
      <div class="errhint">
        {#if call.command !== null}<span class="pfx">$</span>
          {call.command}<br />{/if}
        <div class="m">{piece.message}</div>
        {#if piece.detail !== ''}<div class="d">{piece.detail}</div>{/if}
      </div>
    {:else if asProse}
      <Prose text={piece.text} preserveLines />
    {:else if asCode(piece) !== null}
      <Code path={call.title} text={piece.text} />
    {:else}
      <!-- The command a call ran leads its own output: a call given a
           description shows that as its title, and the command would
           otherwise appear nowhere. The break is an element rather than
           a character in the text, so it is a break whatever the box's
           whitespace rule turns out to be. -->
      <div class="term">
        {#if call.command !== null}<span class="pfx">$</span>
          {call.command}<br />{/if}{piece.text}{#if at === tail && call.note !== null}<br /><span
            class={call.note.tone ?? undefined}>{call.note.text}</span
          >{/if}
      </div>
    {/if}
  {/each}
{/snippet}
