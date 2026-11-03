<script lang="ts">
  import GroupFold from '../components/GroupFold.svelte';
  import { activateLine, installLine, sizeLabel } from '../models/view';
  import type { ActivateState, InstallState, SpeakingWire } from '../wire/models';
  import type { SessionSlot } from '../wire/types';

  /**
   * The voice area's speaking half: the model the role runs, its install and
   * switch controls, and the voices the installed model carries.
   *
   * It draws the models page's own row and list vocabulary rather than a
   * second set of rules for the same facts. **The voice choice is the page's
   * own edit**, staged like every other key and written by the one Save; the
   * preview is the only playback, and it plays on the seat the reader was
   * last on.
   */
  let {
    speaking,
    install,
    activate,
    busy,
    voice,
    seat,
    refusal = null,
    oninstall,
    onactivate,
    ondeactivate,
    onchoose,
    onpreview,
  }: {
    /** The speaking read, or `null` before the models snapshot lands. */
    speaking: SpeakingWire | null;
    install: InstallState;
    activate: ActivateState;
    busy: boolean;
    /** The staged or saved voice id. */
    voice: string | null;
    /** The seat the preview plays on; `null` when the reader has not been on one. */
    seat: SessionSlot | null;
    /** The core's own words for a refused speaking action. */
    refusal?: string | null;
    oninstall: (variant: string) => void;
    onactivate: (file: string) => void;
    ondeactivate: () => void;
    onchoose: (voice: string) => void;
    onpreview: (voice: string) => void;
  } = $props();

  const chosen = $derived(voice ?? 'af_heart');
  const active = $derived(speaking?.active ?? null);
  const enabled = $derived(speaking?.enabled ?? true);
  const downloading = $derived(installLine(install));
  const activating = $derived(activateLine(activate));
</script>

<section class="settings-speaking" aria-labelledby="speaking-title">
  <h3 class="t" id="speaking-title">speaking</h3>

  {#if speaking === null}
    <p class="note">reading the speaking model's state...</p>
  {:else if speaking.from === 'unknown'}
    <p class="note">this forge sent a speaking model state this client cannot read</p>
  {:else}
    {#if !enabled}
      <p class="note">
        speech is off: <code>[tts] enabled</code> is unset, so every reply renders silently. Turn it on
        below and save.
      </p>
    {/if}

    <p class="note">
      {speaking.speaking_model} is installed from the Hugging Face Hub as an ONNX export and the style
      files for its named voices &middot; every file is checked against its digest before the engine loads
      it
    </p>

    {#if active === null}
      <p class="note">no speaking model is chosen; the models page installs one</p>
    {:else}
      <div class="status">
        <span class="dot {active.installed ? 'ok' : ''}"></span>
        <span class="t">{active.variant}</span>
        <span class="when">{active.quant} &middot; {sizeLabel(active.size)}</span>
        <span class="spacer"></span>
        {#if active.installed}
          <span class="chip"><span class="dot ok"></span>active</span>
        {/if}
        {#if enabled}
          {#if !active.installed}
            <button
              class="chip"
              type="button"
              disabled={busy}
              onclick={() => oninstall(active.variant)}>install {active.quant}</button
            >
          {:else if speaking.from === 'installed'}
            <button class="chip" type="button" disabled={busy} onclick={ondeactivate}
              >use the default</button
            >
          {:else if speaking.from === 'pin'}
            <button
              class="chip"
              type="button"
              disabled={busy}
              onclick={() => onactivate(active.file)}>activate</button
            >
          {/if}
        {/if}
      </div>
    {/if}

    {#if downloading !== null}
      <div class="status {downloading.mark === 'ok' ? '' : downloading.mark}" role="status">
        <span class="dot {downloading.mark}"></span>
        <span class="t">{downloading.title}</span>
        {#if downloading.percent !== null}
          <progress max="100" value={downloading.percent} aria-label="download progress"></progress>
        {/if}
        <span class="spacer"></span>
        {#if downloading.detail !== null}<span class="when">{downloading.detail}</span>{/if}
      </div>
    {/if}

    {#if activating !== null}
      <div class="status {activating.mark === 'ok' ? '' : activating.mark}" role="status">
        <span class="dot {activating.mark}"></span>
        <span class="t">{activating.title}</span>
        <span class="spacer"></span>
        {#if activating.detail !== null}<span class="when">{activating.detail}</span>{/if}
      </div>
    {/if}

    {#if speaking.models_dir !== null}
      <p class="note">the files live in <code>{speaking.models_dir}</code></p>
    {/if}

    {#if speaking.voices.length === 0}
      <p class="note">the voices arrive with the model; install it to choose one</p>
    {:else}
      <p class="note">
        the chosen voice is <code>{chosen}</code> &middot; this model carries {speaking.voices
          .length}
        voices, and the choice is saved with the page's other edits
      </p>
      <GroupFold heading="voices" count={speaking.voices.length} startOpen>
        <ul class="list" aria-label="Voices">
          {#each speaking.voices as id (id)}
            <li>
              <span class="nm">{id}</span>
              {#if id === chosen}
                <span class="chip">chosen</span>
              {:else}
                <button class="chip" type="button" onclick={() => onchoose(id)}>use</button>
              {/if}
              <span class="spacer"></span>
              {#if enabled}
                <button
                  class="chip"
                  type="button"
                  disabled={seat === null}
                  title={seat === null ? 'open a session and turn its speaker on' : undefined}
                  onclick={() => onpreview(id)}>preview</button
                >
              {/if}
            </li>
          {/each}
        </ul>
      </GroupFold>
      {#if enabled && seat === null}
        <p class="note">
          a preview plays on the seat you were last on, with its speaker switch on &middot; open a
          session and turn the speaker on, then come back
        </p>
      {/if}
    {/if}
  {/if}

  {#if refusal !== null}<p class="note bad">{refusal}</p>{/if}
</section>
