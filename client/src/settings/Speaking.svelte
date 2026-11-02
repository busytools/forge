<script lang="ts">
  /**
   * The speaking section of the voice area: the model the role runs, the
   * voices the shipped public corpus carries, and one preview per voice.
   *
   * The section is the core's read drawn, not a second config editor: the
   * model's install and activation are the models page's own commands, and
   * the chosen voice is `[tts] voice`, which draws in the rows above.
   */
  import { activateLine, installLine, modelChip, sourceLineFor } from '../models/view';
  import type { DictateModelsWire } from '../wire/models';

  let {
    wire,
    listening = null,
    refusal = null,
    oninstall,
    onactivate,
    ondeactivate,
    onpreview,
  }: {
    /** The models read; `null` until its first snapshot lands. */
    wire: DictateModelsWire | null;
    /** The seat a preview plays through: this client's own voice switch, in
     * its display form. `null` when no seat's switch is on. */
    listening?: string | null;
    /** The core's own words for a refused action. */
    refusal?: string | null;
    oninstall: (variant: string) => void;
    onactivate: (file: string) => void;
    ondeactivate: () => void;
    onpreview: (speaker: string) => void;
  } = $props();

  const speaking = $derived(wire?.speaking ?? null);
  const source = $derived(speaking === null ? '' : sourceLineFor('speaking', speaking.from));
  const busy = $derived(
    wire !== null && (wire.install.state === 'downloading' || wire.activate.state === 'activating'),
  );
  const chip = $derived(speaking === null ? null : modelChip(speaking.state));
  const installing = $derived(wire === null ? null : installLine(wire.install));
  const activating = $derived(wire === null ? null : activateLine(wire.activate));
  const ready = $derived(speaking?.state === 'ready');
  const installed = $derived(speaking?.from.from === 'installed');
  const pinned = $derived(speaking?.from.from === 'config');
</script>

{#snippet op(line: { mark: string; title: string; detail: string | null; percent: number | null })}
  <div class="status {line.mark === 'live' ? '' : line.mark}" role="status">
    <span class="dot {line.mark}"></span>
    <span class="t">{line.title}</span>
    {#if line.percent !== null}
      <progress class="bar" max="100" value={line.percent} aria-label="download progress"
      ></progress>
    {/if}
    <span class="spacer"></span>
    {#if line.detail !== null}<span class="when">{line.detail}</span>{/if}
  </div>
{/snippet}

<section class="settings-speaking" aria-labelledby="settings-speaking-title">
  <h3 class="t" id="settings-speaking-title">speaking model</h3>

  {#if wire === null}
    <p class="settings-empty">the models read has not landed yet</p>
  {:else if speaking === null}
    {#if wire.speaking_error !== null}
      <p class="settings-empty">
        {wire.speaking_error} &middot; fix the key above and restart forge.
      </p>
    {:else}
      <p class="settings-empty">
        speech is off: <code>[tts] enabled</code> is not set, so there is no speaking model to run.
      </p>
    {/if}
  {:else}
    <div class="status">
      <span class="dot {chip?.mark ?? 'off'}"></span>
      <span class="t">{speaking.file}</span>
      <span class="when">{chip?.text ?? ''}</span>
      <span class="spacer"></span>
      {#if !ready}
        <button
          class="chip"
          type="button"
          disabled={busy}
          onclick={() => oninstall(speaking.variant)}>install</button
        >
      {:else if installed}
        <button class="chip" type="button" disabled={busy} onclick={ondeactivate}
          >use the default</button
        >
      {:else if !pinned}
        <button class="chip" type="button" disabled={busy} onclick={() => onactivate(speaking.file)}
          >activate</button
        >
      {/if}
      <span class="detail">{source}</span>
    </div>
    {#if installing !== null}{@render op(installing)}{/if}
    {#if activating !== null}{@render op(activating)}{/if}

    <h4 class="t">voice</h4>
    {#if wire.voice_error !== null}
      <p class="note bad">{wire.voice_error}</p>
    {/if}
    {#if wire.voices.length === 0}
      <p class="note">
        no voices yet &middot; the shipped public corpus is installed beside the speaking model, and
        its speakers land here once it has
      </p>
    {:else}
      <ul class="list" aria-label="Voices">
        {#each wire.voices as voice (voice.speaker)}
          <li>
            <span class="nm">{voice.speaker}</span>
            <span class="col">{voice.seconds}s</span>
            {#if voice.speaker === wire.voice}<span class="chip">chosen</span>{/if}
            <button
              class="chip"
              type="button"
              disabled={busy || listening === null}
              title={listening === null
                ? "turn a seat's voice switch on to hear a preview"
                : `hear this voice through ${listening}`}
              onclick={() => onpreview(voice.speaker)}>preview</button
            >
          </li>
        {/each}
      </ul>
      <p class="note">
        {listening === null
          ? "turn a seat's voice switch on to hear a preview"
          : `previews play through ${listening}`} &middot; a preview renders one fixed line in that voice,
        so a voice can be heard before it is chosen &middot; the choice is
        <code>[tts] voice</code>, beside the model's keys above
      </p>
    {/if}
  {/if}

  {#if refusal !== null}<p class="note bad">{refusal}</p>{/if}
</section>
