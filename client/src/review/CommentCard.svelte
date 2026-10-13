<script lang="ts">
  /**
   * The comment box on a line of the diff: the anchor it was opened on, the
   * words, the two acts, and the mic.
   *
   * The canvas draws it as an inserted row inside the viewer's own table, so
   * it is mounted by hand rather than rendered by the page - but it is a
   * component all the same, which is what lets the box wear the same
   * `DictateControl` the composer's box wears and the same `Field` as every
   * other box in the client.
   *
   * **The words are NOT held here**: the row is rebuilt whenever the canvas
   * moves the box, and a draft this component owned would go with it - the
   * page holds them on the box it hands in, and reads the same box back. The
   * axes and the input ARE held here, seeded from this machine's storage and
   * written back to it, which is what the take the page starts reads at its
   * press.
   */
  import { boxKey } from '../composer/box.svelte';
  import type { Dictation, TakeLine } from '../composer/dictate.svelte';
  import DictateControl from '../composer/DictateControl.svelte';
  import DictationPanel from '../composer/DictationPanel.svelte';
  import Field from '../composer/Field.svelte';
  import type { Bind, Mode } from '../composer/dictate-key';
  import { axesFor, deviceFor, rememberAxes, rememberDevice } from '../composer/dictation';
  import type { ComposerConnection } from '../composer/view';
  import { DEFAULT_AXES, type DictateAxes } from '../session/wire';
  import type { SessionSlot } from '../wire/types';
  import type { ReviewSpot } from './dictate';

  let {
    spot,
    box,
    slot,
    dictate,
    dictation,
    connection = null,
    bind,
    mode,
    oncomment,
    oncancel,
  }: {
    /** The line the box was opened on, which is what its words are filed against. */
    spot: ReviewSpot;
    /** The page's open box: the words, the write in flight, and the take's line. */
    box: { draft: string; busy: boolean; line: TakeLine | null };
    /** The seat whose axes and input this box dictates with. */
    slot: SessionSlot;
    /** The page's take, which the mic drives. */
    dictate: Dictation;
    /** Whether this install can dictate at all, which gates the mic. */
    dictation: boolean;
    connection?: ComposerConnection | null;
    /** The push-to-talk key, whose hint the panel's header carries. */
    bind: Bind;
    /** How a press maps onto a take, which the panel states rather than offers. */
    mode: Mode;
    oncomment: (text: string) => void;
    oncancel: () => void;
  } = $props();

  /** The field, so the panel's close can put the keyboard back in the box. */
  let field = $state<HTMLElement | null>(null);

  /**
   * The axes and input this seat dictates with, held as the composer's box
   * holds its own and written back to this machine's storage: this run's copy
   * of what the panel shows, which a rebuilt row reads back.
   */
  const key = $derived(boxKey(slot));
  const defaults = $derived(connection?.settings()?.dictate ?? DEFAULT_AXES);
  let axesEdit = $state<DictateAxes | null>(null);
  let deviceEdit = $state<string | null>(null);
  const axes = $derived(axesEdit ?? axesFor(key, defaults));
  const device = $derived(deviceEdit ?? deviceFor(key));

  function setAxes(next: DictateAxes): void {
    axesEdit = next;
    rememberAxes(key, next);
  }

  function setDevice(next: string | null): void {
    deviceEdit = next;
    rememberDevice(key, next);
  }

  /**
   * The key that belongs to this box: a live take consumes Esc wherever the
   * focus is, and the settings panel is what the next Escape closes - the
   * same rule the composer's box applies to its own.
   */
  $effect(() => {
    const onDown = (event: KeyboardEvent): void => {
      if (event.key !== 'Escape') return;
      if (dictate.take !== null || dictate.opening || dictate.held) {
        event.preventDefault();
        event.stopPropagation();
        dictate.act('cancel');
        return;
      }
      if (dictate.panel) {
        event.preventDefault();
        event.stopPropagation();
        dictate.panel = false;
        field?.focus();
      }
    };
    window.addEventListener('keydown', onDown, true);
    return () => window.removeEventListener('keydown', onDown, true);
  });
</script>

<article class="rv-thread compact comment">
  <div class="rv-tops">
    <span class="rv-where">{spot.path} · line {spot.line}</span>
  </div>
  <Field
    editor="nowhere"
    class="rv-txarea"
    rows={3}
    placeholder="Comment on this line..."
    bind:value={box.draft}
    field={(el: HTMLElement | null) => {
      field = el;
    }}
  />
  <div class="rv-acts">
    <button
      class="rv-a on"
      type="button"
      disabled={box.busy || box.draft.trim() === ''}
      onclick={() => oncomment(box.draft.trim())}>comment</button
    >
    <button class="rv-a" type="button" onclick={oncancel}>cancel</button>
    {#if dictation}
      <DictateControl {dictate} />
    {/if}
  </div>
  {#if box.line !== null}
    <div class="rv-dictateline {box.line.tone}">{box.line.text}</div>
  {/if}
  {#if dictate.panel}
    <DictationPanel
      {connection}
      {axes}
      {defaults}
      {bind}
      {mode}
      {device}
      onaxes={setAxes}
      ondevice={setDevice}
      onclose={() => {
        dictate.panel = false;
        // The keyboard goes back where Escape puts it, so closing by the
        // button is not a dead end for a keyboard reader.
        field?.focus();
      }}
    />
  {/if}
</article>
