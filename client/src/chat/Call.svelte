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
  const tail = $derived(call.body.map((piece) => piece.kind).lastIndexOf('text'));
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
        {#each call.body as piece, at (at)}
          {#if piece.kind === 'diff'}
            <!-- No header naming the file: the row's own title is the path, and
                 it is the same path, so a line here would print it twice. -->
            <div class="dif">
              {#each piece.old.split('\n') as line, n (`old-${n}`)}
                {#if piece.old !== ''}
                  <div class="ln d">
                    <span class="n">{'\u{2212}'}</span><span class="l">{line}</span>
                  </div>
                {/if}
              {/each}
              {#each piece.new.split('\n') as line, n (`new-${n}`)}
                {#if piece.new !== ''}
                  <div class="ln a"><span class="n">+</span><span class="l">{line}</span></div>
                {/if}
              {/each}
            </div>
          {:else if piece.kind === 'image'}
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
      {/if}
    </div>
  {/if}
</details>
