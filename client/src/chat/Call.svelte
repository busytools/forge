<script lang="ts">
  import { untrack } from 'svelte';

  import Chevron from '../components/Chevron.svelte';
  import Icon from '../components/Icon.svelte';
  import Code from './Code.svelte';
  import { languageFor, type CallBody, type ToolLeaf } from './leaves';
  import { searchHits } from './text';

  /**
   * One call: what it was, whether it came back, and what it came back with.
   *
   * The group above keys this row by the row's own key, which for a call is
   * the id the wire gave it. A place in a lane is not a key: a page landing
   * with its copy of the turn can put a block above this row, and every place
   * below it shifts - which remounts the row and closes what the reader had
   * open in it.
   *
   * `open` is what the row's own kind decides - a mutation's diff is drawn
   * without being asked - and the reader's own toggling takes it from there.
   */
  let { call, open = false }: { call: ToolLeaf; open?: boolean } = $props();

  /**
   * Whether the row is open, held HERE rather than drawn from the prop.
   *
   * The prop says where the row starts - a mutation's diff is open without
   * being asked - and the element owns the state from then on: a row is
   * re-rendered whenever the turn is, and an `open` attribute written from a
   * prop on every update closes a row the reader has just opened.
   */
  let opened = $state(untrack(() => open));

  /** The tools whose body is a list of hits rather than prose or a command's output. */
  const SEARCHES = new Set(['Grep', 'Glob', 'LS']);

  /** The hits a search call came back with, or `null` when this is not one. */
  const hits = $derived(
    SEARCHES.has(call.name)
      ? searchHits(call.body.map((piece) => (piece.kind === 'text' ? piece.text : '')).join('\n'))
      : null,
  );

  /** The language a read's body is drawn in, when the call named a file. */
  function asCode(piece: CallBody): string | null {
    return piece.kind === 'text' ? languageFor(call) : null;
  }

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
</script>

<details
  class="leaf"
  class:running={call.status === 'in_progress'}
  bind:open={opened}
  data-k={`call-${call.id}`}
>
  <summary>
    {#if call.status === 'completed'}
      <Icon name="check" class="st" />
    {:else if call.status === 'failed' || call.status === 'killed'}
      <Icon name="x" class="st err" />
    {:else}
      <span class="st"><span class="ring"></span></span>
    {/if}
    <span class="tn">{call.title}</span>
    <Chevron />
  </summary>

  {#if call.body.length > 0}
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
          <div class="dif" class:added>
            {#each patches as piece, at (at)}
              {#if piece.kind === 'diff'}
                <!-- No header naming the file: the row's own title is the path,
                     and it is the same path, so a line here would print it
                     twice. -->
                {#each piece.old.split('\n') as line, n (`old-${n}`)}
                  {#if piece.old !== ''}
                    <div class="ln d">
                      <span class="on"></span><span class="nn"></span><span class="n"
                        >{'\u{2212}'}</span
                      ><span class="l">{line}</span>
                    </div>
                  {/if}
                {/each}
                {#each piece.new.split('\n') as line, n (`new-${n}`)}
                  {#if piece.new !== ''}
                    <div class="ln a">
                      <span class="on"></span><span class="nn"></span><span class="n">+</span><span
                        class="l">{line}</span
                      >
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
        {#each rest as piece, at (at)}
          {#if piece.kind === 'image'}
            <div class="term">
              image{#if piece.mime}{' \u{b7} '}{piece.mime}{/if}{#if piece.uri}{' \u{b7} '}{piece.uri}{/if}
            </div>
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
                {call.command}<br />{/if}{piece.text}{#if at === tail && call.note !== null}<br
                /><span class={call.note.tone ?? undefined}>{call.note.text}</span>{/if}
            </div>
          {/if}
        {/each}
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
