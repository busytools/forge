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
  let stage = $state<HTMLElement | null>(null);
  let frame = $state<TakeoverFrame | null>(null);
  /**
   * The CSS size the browser's own viewport is being told to be.
   *
   * **The page is sized to the screen, not to some default.** Left alone, the
   * headless browser renders at 800x600 and the stage shows a small page in a
   * void; overriding the device metrics to the stage's own box makes the
   * page fill it - at the display's scale, so the frames are crisp - and the
   * input mapping is one rectangle either way.
   */
  let viewport = $state<{ width: number; height: number } | null>(null);
  /** The decoded last frame; drawn when it loads, so frames never flicker. */
  const image = new Image();

  function draw(): void {
    // jsdom hands back no context; the live view is exercised against the
    // real engine, and everything around this call is what tests drive.
    canvas?.getContext('2d')?.drawImage(image, 0, 0);
  }

  /** Tell the browser how big its viewport is, so the page fills the stage. */
  function fit(): void {
    const el = stage;
    if (el === null) return;
    const width = Math.round(el.clientWidth);
    const height = Math.round(el.clientHeight);
    if (width <= 0 || height <= 0) return;
    if (viewport?.width === width && viewport.height === height) return;
    viewport = { width, height };
    void takeoverInput('Emulation.setDeviceMetricsOverride', {
      width,
      height,
      deviceScaleFactor: Math.max(1, Math.round(window.devicePixelRatio || 1)),
      mobile: false,
    });
  }

  // The stage's own size, now and whenever the window changes: the viewport
  // follows it. jsdom has no ResizeObserver and lays nothing out, so the
  // guard keeps tests to the wiring they can see.
  $effect(() => {
    fit();
    if (typeof ResizeObserver === 'undefined') return;
    const el = stage;
    if (el === null) return;
    const observer = new ResizeObserver(() => fit());
    observer.observe(el);
    return () => observer.disconnect();
  });

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
    if (el === null) return null;
    // The page's CSS size is the viewport we asked for; before the first
    // override lands, the frame's own pixels are what the canvas holds.
    const page = viewport ?? (frame === null ? null : { width: frame.width, height: frame.height });
    if (page === null) return null;
    return toPage(
      { stage: el.getBoundingClientRect(), page },
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
  <div class="stage" bind:this={stage}>
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
