<script lang="ts">
  import Chevron from '../components/Chevron.svelte';
  import Icon from '../components/Icon.svelte';
  import type { Notice } from './units';

  /**
   * A line nobody typed: an external delivery, a scheduled fire, or a failure
   * the workspace reported.
   *
   * **The row is a tool row, and the level is its glyph.** Three shapes - a
   * circle, a triangle, a circle with an exclamation - carry info, warning
   * and error where a bold word used to, because a notice sits among the calls
   * and a second system inside the group reads as one (Ved, 2026-10-30). The
   * shapes differ, so the level is not carried by colour alone, and the
   * glyph's own name says it to a screen reader.
   *
   * **The words wrap rather than clamp.** A call's title may ellipsize because
   * the call still opens onto its input; a notice's text IS the message, and a
   * narrow row that clipped it would leave the retry reason or a failure's
   * next steps unreadable. A plain notice therefore is not a disclosure at
   * all: it is the closed row, static, with no chevron to promise a body.
   *
   * **A provisional notice opens onto the frame's own facts.** A frame this
   * build has no face for yet draws as itself, marked with its own type, and
   * the open body lists what the frame carried as labelled values - the raw
   * JSON was the first cut's face, and a reader asked for the facts instead
   * (Ved, 2026-10-28). The frame itself stays one control further in, so the
   * decision about its real face is still made from what arrived (rule 25).
   */
  let { notice }: { notice: Notice } = $props();

  const glyph = $derived(
    notice.severity === 'warning' ? 'warning' : notice.severity === 'error' ? 'error' : 'info',
  );
  const tone = $derived(
    notice.severity === 'warning' ? ' warn' : notice.severity === 'error' ? ' err' : '',
  );
  /** Only a provisional row has a body, so only it is a disclosure. */
  const opens = $derived(notice.raw !== undefined);

  /** The raw frame is only rendered once its own control is open. */
  let rawOpen = $state(false);
</script>

{#snippet head()}
  <!-- The glyph and the text travel as one piece: when the row is narrow the
       text wraps under its own glyph rather than leaving the glyph alone on a
       line, and the tails fall after the group. -->
  <span class="lead">
    <Icon name={glyph} class={`gl${tone}`} label={notice.severity} />
    <span class="tn">{notice.text}</span>
  </span>
  {#if notice.mark !== undefined}
    <span class="chip">{notice.mark}</span>
  {/if}
  {#if notice.chip !== undefined}
    <span class="chip">{notice.chip}</span>
  {/if}
  {#if notice.sub !== undefined}
    <span class="sub2">{notice.sub}</span>
  {/if}
{/snippet}

{#if opens}
  <details class="leaf nrow">
    <summary>
      {@render head()}
      <Chevron />
    </summary>
    <div class="body">
      <div class="nbody">
        {#each notice.fields ?? [] as field, index (`${index}:${field.label}`)}
          <div class="nfact">
            <span class="k">{field.label}</span><span class="v">{field.value}</span>
          </div>
        {/each}
        <details class="raw" bind:open={rawOpen}>
          <summary><Chevron />raw frame</summary>
          {#if rawOpen}<pre>{notice.raw}</pre>{/if}
        </details>
      </div>
    </div>
  </details>
{:else}
  <div class="leaf nrow">
    <div class="rrow">{@render head()}</div>
  </div>
{/if}
