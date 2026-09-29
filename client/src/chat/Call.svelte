<script lang="ts">
  import Chevron from '../components/Chevron.svelte';
  import Icon from '../components/Icon.svelte';
  import { languageOf } from './code';
  import Code from './Code.svelte';
  import { opensByDefault, type CallBody, type CallRow } from './rows';
  import { searchHits } from './text';

  /**
   * One call: what it was, whether it came back, and what it came back with.
   *
   * The row is keyed by the call's own id on the turn above, so growing one
   * call does not redraw the calls around it - which is what keeps a row a
   * reader has opened open while the turn is still being written.
   */
  let { call }: { call: CallRow } = $props();

  /** The tools whose body is a list of hits rather than prose or a command's output. */
  const SEARCHES = new Set(['Grep', 'Glob', 'LS']);

  /** Whether this row's body is drawn without being asked for. */
  const opens = $derived(opensByDefault(call.name));

  /** The hits a search call came back with, or `null` when this is not one. */
  const hits = $derived(
    SEARCHES.has(call.name)
      ? searchHits(call.body.map((piece) => (piece.kind === 'text' ? piece.text : '')).join('\n'))
      : null,
  );

  /** The language a read's body is drawn in, when the call named a file. */
  function asCode(piece: CallBody): string | null {
    return call.name === 'Read' && piece.kind === 'text' ? languageOf(call.title) : null;
  }
</script>

<details class="leaf" open={opens} data-k={`call-${call.id}`}>
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
            <div class="dif">
              <div class="h">{piece.path}</div>
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
                {call.command}<br />{/if}{piece.text}
            </div>
          {/if}
        {/each}
      {/if}
    </div>
  {/if}
</details>
