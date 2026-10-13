<script lang="ts">
  /**
   * A canvas that can hand the comment card a new box, which is what a test
   * needs and `mount` alone cannot do.
   *
   * **The box lives here in `$state`, and that is the point**: the page owns
   * the words and the line, exactly as the canvas does - a card that held
   * them itself would be testing a component that does not exist in the app.
   * Test support rather than a page: nothing the app ships imports this file.
   */
  import type { Dictation, TakeLine } from '../composer/dictate.svelte';
  import type { ComposerConnection } from '../composer/view';
  import type { SessionSlot } from '../wire/types';
  import CommentCard from './CommentCard.svelte';

  let {
    dictate,
    slot,
    connection = null,
    line = null,
    busy = false,
    dictation = true,
    oncomment,
    oncancel,
  }: {
    dictate: Dictation;
    slot: SessionSlot;
    connection?: ComposerConnection | null;
    line?: TakeLine | null;
    busy?: boolean;
    dictation?: boolean;
    oncomment: (text: string) => void;
    oncancel: () => void;
  } = $props();

  const spot = { path: 'client/src/review/write.ts', side: 'new' as const, line: 199 };
  const box = $state({ draft: '', busy: false, line: null as TakeLine | null });
  $effect(() => {
    box.busy = busy;
    box.line = line;
  });
</script>

<CommentCard
  {spot}
  {box}
  {slot}
  {dictate}
  {dictation}
  {connection}
  bind="right_cmd"
  mode="auto"
  {oncomment}
  {oncancel}
/>
