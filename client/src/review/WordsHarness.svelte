<script lang="ts">
  /**
   * A page that can hand one words box a live take, which is what a test needs
   * and `mount` alone cannot do.
   *
   * **It keeps the page's two halves of the bargain**, so the test drives the
   * real thing: the box is the one the take belongs to exactly when its own
   * mic was pressed, and the panel is drawn by the box whose mic was held -
   * the rules the review page applies through its own `takeIn` and `panelIn`.
   * Test support rather than a page: nothing the app ships imports this file.
   */
  import { DEFAULT_AXES } from '../session/wire';
  import {
    takePhase,
    takeSilenceLine,
    type Dictation,
    type TakeLine,
  } from '../composer/dictate.svelte';
  import type { ComposerConnection } from '../composer/view';
  import type { Take } from '../composer/wire';
  import type { SessionSlot } from '../wire/types';
  import type { CommentBox, Voice } from './boxes';
  import WordsBox from './WordsBox.svelte';

  let {
    dictate,
    slot,
    connection = null,
    take = null,
    dictation = true,
    over = null,
    onsend,
    oncancel,
  }: {
    dictate: Dictation;
    slot: SessionSlot;
    connection?: ComposerConnection | null;
    /** The record's take, when the take the box watches is the record's own. */
    take?: Take | null;
    dictation?: boolean;
    over?: { line?: TakeLine | null; busy?: boolean } | null;
    onsend: (text: string) => void;
    oncancel: () => void;
  } = $props();

  /** Whether a take begun here is this box's, stamped exactly as the page
   *  stamps it: the press NAMES the box, and the take follows the box. */
  let mine = $state(false);
  let mypanel = $state(false);

  const box = $state<CommentBox>({
    spot: { path: 'client/src/review/write.ts', side: 'new', line: 199 },
    draft: '',
    busy: false,
    line: null,
    phase: null,
    silent: null,
    panel: false,
    take: null,
    wire: null,
  });
  // What the box reads off its own state, stamped exactly as the page stamps
  // it: the press NAMES the box, and the take follows the box.
  $effect(() => {
    box.busy = over?.busy ?? false;
    box.line = over?.line ?? null;
    box.take = take;
    box.wire = dictate.take === null ? null : dictate.take.wire;
    box.phase = mine ? takePhase(dictate, take, slot) : null;
    box.silent = mine ? (takeSilenceLine(dictate, slot)?.text ?? null) : null;
    box.panel = dictate.panel && mypanel;
  });

  const voice: Voice = {
    get dictate() {
      return dictate;
    },
    get offered() {
      return dictation;
    },
    get connection() {
      return connection;
    },
    bind: 'right_cmd',
    mode: 'auto',
    axes: DEFAULT_AXES,
    device: null,
    defaults: DEFAULT_AXES,
    onaxes: () => undefined,
    ondevice: () => undefined,
  };
</script>

<WordsBox
  class="rv-thread compact comment"
  headline="{box.spot?.path ?? ''} · line {box.spot?.line ?? 0}"
  rows={3}
  placeholder="Comment on this line..."
  label="your comment"
  sendLabel="comment"
  {box}
  {voice}
  {onsend}
  {oncancel}
  onpress={() => (mine = true)}
  onhold={() => (mypanel = true)}
/>
