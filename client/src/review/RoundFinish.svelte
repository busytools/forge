<script lang="ts">
  /**
   * The finish stack: the round's summary over the sealing button, each on
   * its own row, the mic at the row's end - **one draw for the review page
   * and the files rail** (the lead's gate, 2026-10-17: the rail reproduced
   * the review's block for block). The page owns the words, the count and
   * the write; this owns the arrangement.
   */
  import DictateControl from '../composer/DictateControl.svelte';
  import DictationPanel from '../composer/DictationPanel.svelte';
  import type { RowPhase, TakeLine } from '../composer/dictate.svelte';
  import Field from '../composer/Field.svelte';
  import TakeCard from '../composer/TakeCard.svelte';
  import type { Take } from '../composer/wire';
  import type { LiveWire, Voice } from '../review/boxes';

  let {
    draft = $bindable(''),
    phase = null,
    take = null,
    wire = null,
    silent = null,
    line = null,
    panel = false,
    voice,
    count,
    complete,
    busy,
    onfinish,
    oncancel,
    onpress,
    onhold,
    onfield,
    onclosed = () => {},
  }: {
    /** The summary as written. */
    draft?: string;
    /** The take's phase while one runs in this box, or null. */
    phase: RowPhase | null;
    /** The record's take, and the counts the page produced for it. */
    take?: Take | null;
    wire?: LiveWire | null;
    /** What the take's card says about a device feeding nothing. */
    silent?: string | null;
    /** The take's own line, when the page owes one here. */
    line?: TakeLine | null;
    /** Whether this box's mic opened the settings panel. */
    panel?: boolean;
    voice: Voice;
    /** The unfiled turns a round would seal, for the button's own count. */
    count: number;
    /** Whether sealing is possible at all: the button's own gate. */
    complete: boolean;
    busy: boolean;
    /** Seal the round, with the summary as written. */
    onfinish: () => void;
    /** A live take's cancel, which may drop the page's landing arm too. */
    oncancel: () => void;
    onpress: () => void;
    onhold: () => void;
    onfield: (el: HTMLElement | null) => void;
    /** The settings panel closed: the page puts the keyboard back where its
     *  own Escape would. */
    onclosed?: () => void;
  } = $props();
</script>

<div class="rv-finishstack">
  <div class="rv-wordline line" class:takeline={phase !== null}>
    {#if phase !== null}
      <TakeCard {take} {wire} {phase} {silent} {oncancel} spread />
    {:else}
      <Field
        editor="nowhere"
        class="rv-txarea"
        rows={4}
        placeholder="summary (optional)"
        aria={{ label: 'the round summary' }}
        bind:value={draft}
        onkeydown={(event: KeyboardEvent) => {
          // **Enter finishes the review**, as its own button does: the same
          // key sends everywhere, and the same guard keeps a round with
          // nothing filed from sealing (Ved, 2026-10-15).
          if (event.key !== 'Enter' || event.shiftKey) return;
          event.preventDefault();
          if (!busy && complete) onfinish();
        }}
        field={onfield}
      />
    {/if}
  </div>
  <div class="rv-finishacts">
    <button class="rv-finish" type="button" disabled={busy || !complete} onclick={onfinish}>
      {count > 0 ? `finish review (${count})` : 'finish review'}
    </button>
    {#if voice.offered}
      <DictateControl dictate={voice.dictate} {onpress} {onhold} />
    {/if}
    {#if panel}
      <DictationPanel
        connection={voice.connection}
        axes={voice.axes}
        defaults={voice.defaults}
        bind={voice.bind}
        mode={voice.mode}
        device={voice.device}
        onaxes={voice.onaxes}
        ondevice={voice.ondevice}
        onclose={() => {
          voice.dictate.panel = false;
          onclosed();
        }}
      />
    {/if}
  </div>
  {#if line !== null}
    <div class="rv-dictateline {line.tone}">{line.text}</div>
  {/if}
</div>
