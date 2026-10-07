<script lang="ts">
  /**
   * The takeover: the client's screen replaced by the browser, with the bar
   * that gets you back.
   *
   * **The stage is the browser.** The shell streams frames of the very
   * browser the sessions drive (its CDP screencast) and this component draws
   * them into a canvas at the page's own pixel size; pointer, wheel and
   * keyboard events go back down the same connection, mapped into the page's
   * coordinates. The bar, the way back and Done are the web side's.
   */
  import { onTakeoverFrame, takeoverFrame, takeoverInput, type TakeoverFrame } from './host';
  import { modifiers, toPage } from './input';
  import { takeover } from './takeover.svelte';

  let { address = '' }: { address?: string } = $props();

  let canvas = $state<HTMLCanvasElement | null>(null);
  let frame = $state<TakeoverFrame | null>(null);
  /** The decoded last frame; drawn when it loads, so frames never flicker. */
  const image = new Image();

  function draw(): void {
    // jsdom hands back no context; the live view is exercised against the
    // real engine, and everything around this call is what tests drive.
    canvas?.getContext('2d')?.drawImage(image, 0, 0);
  }

  /** Draw a frame whenever one lands: the store update and the decode. */
  function show(held: TakeoverFrame): void {
    frame = held;
    // The backing store is the page's own pixels the moment a frame lands;
    // the draw waits for the decode so nothing draws half an image.
    const el = canvas;
    if (el !== null && held.width > 0 && held.height > 0) {
      if (el.width !== held.width) el.width = held.width;
      if (el.height !== held.height) el.height = held.height;
    }
    image.src = `data:image/png;base64,${held.data}`;
  }

  $effect(() => {
    image.onload = () => draw();
    let stop: (() => void) | null = null;
    let gone = false;
    // What is current first - the stream may have delivered before this
    // screen could listen - then every frame after it.
    void takeoverFrame().then((held) => {
      if (held !== null) show(held);
    });
    void onTakeoverFrame(show).then((off) => {
      if (gone) off();
      else stop = off;
    });
    return () => {
      gone = true;
      stop?.();
    };
  });

  /** A client point in the page's pixels, and the geometry that mapped it. */
  function at(event: { clientX: number; clientY: number }): { x: number; y: number } | null {
    const el = canvas;
    const held = frame;
    if (el === null || held === null) return null;
    return toPage(
      { stage: el.getBoundingClientRect(), page: { width: held.width, height: held.height } },
      { x: event.clientX, y: event.clientY },
    );
  }

  function mouse(event: PointerEvent, type: 'mousePressed' | 'mouseMoved' | 'mouseReleased'): void {
    const point = at(event);
    if (point === null) return;
    const buttons = type === 'mouseReleased' ? 0 : 1;
    const click = type === 'mouseMoved' ? { buttons } : { buttons, button: 'left', clickCount: 1 };
    void takeoverInput('Input.dispatchMouseEvent', { type, x: point.x, y: point.y, ...click });
  }

  function wheel(event: WheelEvent): void {
    const point = at(event);
    if (point === null) return;
    event.preventDefault();
    void takeoverInput('Input.dispatchMouseEvent', {
      type: 'mouseWheel',
      x: point.x,
      y: point.y,
      deltaX: event.deltaX,
      deltaY: event.deltaY,
    });
  }

  function key(event: KeyboardEvent, type: 'keyDown' | 'keyUp'): void {
    // Escape is the way back, above any page that wants it.
    if (event.key === 'Escape') return;
    event.preventDefault();
    void takeoverInput('Input.dispatchKeyEvent', {
      type,
      key: event.key,
      code: event.code,
      modifiers: modifiers(event),
      ...(type === 'keyDown' && event.key.length === 1 ? { text: event.key } : {}),
    });
  }
</script>

<svelte:window
  onkeydown={(event: KeyboardEvent) => {
    if (event.key === 'Escape' && takeover.active) void takeover.back();
  }}
/>

<div class="takeover">
  <div class="bar">
    <button type="button" class="back" onclick={() => void takeover.back()}>
      <span class="arw">←</span> back to forge
    </button>
    <span class="addr">{address}</span>
    {#if takeover.asking !== null}
      <button type="button" class="done" onclick={() => void takeover.done()}>Done</button>
    {/if}
  </div>
  <!-- The stage: the page's own pixels, drawn from the shell's frames. -->
  <div class="stage">
    <canvas
      bind:this={canvas}
      tabindex="0"
      aria-label="the browser"
      onpointerdown={(event: PointerEvent) => mouse(event, 'mousePressed')}
      onpointermove={(event: PointerEvent) => mouse(event, 'mouseMoved')}
      onpointerup={(event: PointerEvent) => mouse(event, 'mouseReleased')}
      onwheel={wheel}
      onkeydown={(event: KeyboardEvent) => key(event, 'keyDown')}
      onkeyup={(event: KeyboardEvent) => key(event, 'keyUp')}
    ></canvas>
  </div>
</div>
