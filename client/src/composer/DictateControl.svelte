<script lang="ts">
  /**
   * The mic button both surfaces that take typing wear: the press that starts
   * and ends a take, and the hold that opens what dictation is set to.
   *
   * A press means what the surface's `Dictation` says it means - this owns
   * the gestures alone. Two things deliberately stay with the surface: the
   * settings panel, which is anchored to whichever box the surface draws the
   * control in (the composer's whole surface, the review's comment card), and
   * the Escape that closes it, which has to be live even while this button is
   * not drawn (the composer draws its dock in this control's place while a
   * prompt holds the slot).
   *
   * The mic is the trigger and the door both: a press starts or ends a take -
   * a phone has no push-to-talk key, so a mic that only SHOWED the settings
   * would leave a finger no way to dictate - and a hold opens what dictation
   * is set to (Ved, 2026-10-08: press once to dictate, hold for settings).
   */
  import Icon from '../components/Icon.svelte';
  import type { Dictation } from './dictate.svelte';

  let {
    dictate,
    onpress = () => {},
    onhold = () => {},
    element = () => {},
  }: {
    /** The take this surface's mic drives. */
    dictate: Dictation;
    /** The reader's last act was on the mic, which is what keeps the caret on it. */
    onpress?: () => void;
    /**
     * The settings were opened from this control, which a surface with more
     * than one box needs: the panel is anchored to whichever box drew the
     * control, so the box that was held is the one that draws it.
     */
    onhold?: () => void;
    /**
     * The button, wherever the surface puts it - the composer moves it between
     * rows, so a surface that keeps the caret on it needs the element itself
     * rather than a name to look up.
     */
    element?: (el: HTMLElement | null) => void;
  } = $props();

  /** A hold on the mic that opens the settings instead of firing a take. */
  const MIC_HOLD_MS = 450;
  /**
   * How long after a hold fires its trailing click may still arrive. The
   * swallow must expire: a hold whose click never reaches the button (a drag
   * off, or a touch long-press whose platform sends none) would otherwise eat
   * the NEXT keyboard or assistive activation of the mic.
   */
  const MIC_SWALLOW_MS = 800;
  let holdTimer: ReturnType<typeof setTimeout> | null = null;
  /** When the last hold fired, so its release does not also read as a press. */
  let heldAt = 0;
  let micEl = $state<HTMLElement | null>(null);

  // Through an effect rather than `bind:this`, because the handle is a callback
  // the surface keeps rather than a variable this component owns - and the
  // teardown is reported, as the shared field's is: a surface that swaps the
  // control out must not keep a handle to a detached node.
  $effect(() => {
    element(micEl);
    return () => {
      element(null);
    };
  });

  function press(): void {
    if (heldAt !== 0 && Date.now() - heldAt < MIC_SWALLOW_MS) {
      heldAt = 0;
      return;
    }
    heldAt = 0;
    onpress();
    dictate.toggle();
  }

  function holdStart(event: PointerEvent): void {
    // Primary button only: a right-click hold is not a hold.
    if (event.button !== 0) return;
    heldAt = 0;
    holdTimer = setTimeout(() => {
      holdTimer = null;
      heldAt = Date.now();
      onhold();
      dictate.panel = true;
    }, MIC_HOLD_MS);
  }

  function holdEnd(): void {
    if (holdTimer !== null) {
      clearTimeout(holdTimer);
      holdTimer = null;
    } else if (heldAt !== 0) {
      // The hold fired, so its release is when the trailing click arrives:
      // re-anchor the swallow here, or a hold longer than its window would
      // start a take on release.
      heldAt = Date.now();
    }
  }
</script>

<button
  class="mic"
  type="button"
  bind:this={micEl}
  title="press to dictate, hold or Shift+Enter for settings"
  aria-label={dictate.take === null && !dictate.held ? 'dictate' : 'stop dictating'}
  aria-expanded={dictate.panel}
  onpointerdown={holdStart}
  onpointerup={holdEnd}
  onpointerleave={holdEnd}
  onkeydown={(event: KeyboardEvent) => {
    // **The hold's door for a keyboard.** The press is the button's
    // own click; the settings need one modifier shape, said in the
    // title so it is discoverable rather than folklore.
    if (event.key === 'Enter' && event.shiftKey) {
      event.preventDefault();
      onhold();
      dictate.panel = true;
    }
  }}
  onclick={press}
>
  <Icon name="mic" />
</button>
