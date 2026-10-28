<script lang="ts">
  /**
   * The browser takeover's page area: the whole screen under the dock's bar.
   *
   * **The bar is the dock's** - it carries the hand-off's actions and the
   * feedback line, with the same words row, take card and mic as every other
   * box. This component is the picture and the states before a frame lands.
   */
  import { onMount } from 'svelte';

  import type { BrowserInputEvent, InputModifiers } from '../protocol';
  import type { BrowserFrame, Connection } from '../socket';
  import { drawsCanvas, keptFrame } from './link';
  import { pagePoint } from './point';
  import {
    claimSurfaceMount,
    closeSurface,
    holdsControl,
    pageArea,
    setRegion,
    surface,
    surfaceMountIsCurrent,
  } from './surface.svelte';

  let { connection }: { connection: Connection } = $props();

  let pageEl: HTMLDivElement;

  /** The page's own picture, on a platform with no native overlay. */
  let canvasEl = $state<HTMLCanvasElement | null>(null);
  const painting = drawsCanvas();

  /**
   * **The phone paints the frames itself.** The server relays each one to
   * this page (a WebP still); the canvas takes it whole, and a frame that
   * cannot be decoded is dropped rather than tearing the surface down - the
   * next paint is a fresh picture.
   */
  $effect(() => {
    if (!painting) return;
    return connection.onBrowserFrame((frame) => void paint(frame));
  });

  // **A surface that mounts late asks for the newest frame.** The attach's
  // key frame lands while this component is still `opening` - the canvas
  // exists only in the live branch - and a still page never repaints; the
  // shell keeps the newest one, so the ask waits for the canvas.
  $effect(() => {
    if (!painting || surface.phase !== 'live') return;
    // **A tab switch is a new picture**: the previous tab's last frame stays
    // in the shell until another arrives, so the kept frame is asked again
    // for the tab that is now viewed.
    void surface.tab;
    void askKept();
  });

  /**
   * **The kept-frame ask is retried.** It can land before the canvas is
   * bound, or before the attach's key frame reaches the shell; a single ask
   * that missed left a still page blank for good. Ten tries at 100 ms cover
   * the mount and the attach, and stop at the first frame drawn.
   */
  async function askKept(tries = 0): Promise<void> {
    if (surface.phase !== 'live') return;
    const frame = await keptFrame();
    if (frame !== null && (await paint(frame))) return;
    if (tries >= 10) return;
    await new Promise((resolve) => setTimeout(resolve, 100));
    if (surface.phase === 'live') await askKept(tries + 1);
  }

  /** The newest sequence the canvas drew, per tab: an older frame for any
   *  tab must not paint over the one already there. */
  const painted: Record<string, number> = {};

  /**
   * **The page the last painted frame shows**, in its own CSS pixels: the
   * space a gesture is sent in. Not the canvas box and not this surface's
   * area - an agent's `browser_resize` under the controller lays the page
   * out at another size, and the picture is letterboxed in this one.
   */
  let picturePage = $state<{ width: number; height: number } | null>(null);

  /** Draw one frame, answering whether it became the picture. */
  async function paint(frame: BrowserFrame): Promise<boolean> {
    const canvas = canvasEl;
    if (canvas === null) return false;
    // **Only the viewed tab paints, and only once both ids are named.** A
    // frame that raced the attach (the previous profile's or tab's, after a
    // re-attach) would otherwise paint under a null id; the kept-frame ask
    // waits for `live`, which is the same `browser_view` that sets them.
    if (
      surface.id === null ||
      surface.tab === null ||
      frame.profile !== surface.id ||
      frame.tab !== surface.tab
    ) {
      console.debug(
        'a frame for another tab was dropped',
        frame.profile,
        frame.tab,
        surface.id,
        surface.tab,
      );
      return false;
    }
    // **A relayed frame can overtake an older one** on the way through the
    // page; painting it would put a stale picture over the newest. The tab's
    // own last sequence is the one it compares against.
    const key = `${frame.profile}:${frame.tab}`;
    const drawn = painted[key];
    if (drawn !== undefined && frame.seq <= drawn) return false;
    let bitmap: ImageBitmap;
    try {
      // A fresh backing buffer: the frame's view may be a slice of a shared
      // one, which a Blob refuses.
      const part = new Uint8Array(frame.bytes);
      bitmap = await createImageBitmap(new Blob([part], { type: 'image/webp' }));
    } catch (why) {
      // **A frame this page cannot decode is said**, not dropped quietly:
      // the desktop's overlay logs the same event.
      console.debug(
        'a relayed frame did not decode (event_name client_browser_frame_undecodable)',
        frame.width,
        frame.height,
        why,
      );
      return false;
    }
    // **The decode is an await, and a newer paint can land inside it.** The
    // sequence is checked again here or the older decode finishes last and
    // paints over the newer picture.
    const now = painted[key];
    if (now !== undefined && frame.seq <= now) {
      bitmap.close();
      return false;
    }
    canvas.width = frame.width;
    canvas.height = frame.height;
    canvas.getContext('2d')?.drawImage(bitmap, 0, 0);
    bitmap.close();
    painted[key] = frame.seq;
    picturePage = { width: frame.page_width, height: frame.page_height };
    return true;
  }

  /** The picture a gesture is measured against: its box on screen, and the
   *  page it shows.
   *
   *  **The canvas is scaled to fit and may be letterboxed**, so its element's
   *  pixels are not always the page's: the frame names the page it was
   *  painted from, and every measure maps through both. Before a frame lands
   *  the surface's own area is the only page known. */
  function picture(): { box: DOMRect; page: { width: number; height: number } } | null {
    const canvas = canvasEl;
    const page = picturePage ?? pageArea();
    if (canvas === null || page === null) return null;
    return { box: canvas.getBoundingClientRect(), page };
  }

  /** A gesture's point, in the PAGE's own CSS pixels. */
  function point(event: { clientX: number; clientY: number }): { x: number; y: number } {
    const shown = picture();
    if (shown === null) return { x: 0, y: 0 };
    return pagePoint(event.clientX, event.clientY, shown.box, shown.page);
  }

  function modifiers(event: {
    altKey: boolean;
    ctrlKey: boolean;
    metaKey: boolean;
    shiftKey: boolean;
  }): InputModifiers {
    return {
      alt: event.altKey,
      control: event.ctrlKey,
      meta: event.metaKey,
      shift: event.shiftKey,
    };
  }

  /** **Only the controller's gestures go out**: the server refuses the rest,
   *  and the bar's Take control is the door to the role. */
  function send(action: 'move' | 'press' | 'release', event: PointerEvent): void {
    if (!holdsControl()) return;
    const { x, y } = point(event);
    const gesture: BrowserInputEvent = {
      kind: 'mouse',
      action,
      x,
      y,
      button: 0,
      // **A double click is the count the DOM already decided**, so the page
      // gets the second click as one rather than two singles.
      clicks: event.detail > 0 ? event.detail : 1,
      modifiers: modifiers(event),
    };
    connection.browserInput(gesture);
  }

  /** One-finger state, for the phone's own gestures. */
  let touch: {
    id: number;
    x: number;
    y: number;
    lastX: number;
    lastY: number;
    moved: boolean;
  } | null = null;
  /** The field the phone's keyboard types into. */
  let imeEl = $state<HTMLInputElement | null>(null);
  /**
   * The text the last composition committed. **Some platforms re-fire it as
   * an `input` after `compositionend`**; matching it once keeps the page from
   * receiving the same words twice. **The latch is one turn's**: a later
   * direct insert of the same string is not the commit re-firing, so the
   * latch is cleared on the next task rather than kept until a mismatch.
   *
   * **Not device-verified**: the emulator run drove typed text, Backspace and
   * swipe; the composition path itself is not.
   */
  let composedText: string | null = null;

  /**
   * **The keyboard belongs to the page while this surface holds the role.**
   * The canvas is not focusable, so a click on it moves focus to the body and
   * every key is lost until the role changes - measured on the web.
   */
  function takeKeyboard(): void {
    imeEl?.focus({ preventScroll: true });
    // **The browser clears the focus its own gesture handling did not set**;
    // the next frame puts it back (measured on the dev stack).
    const refocus = (): void => {
      if (holdsControl() && document.activeElement !== imeEl) {
        imeEl?.focus({ preventScroll: true });
      }
    };
    if (typeof requestAnimationFrame === 'undefined') refocus();
    else requestAnimationFrame(refocus);
  }

  function press(event: PointerEvent): void {
    if (event.pointerType === 'touch') {
      if (!holdsControl()) return;
      // One finger at a time: a second finger's press is not a new gesture.
      if (touch !== null) return;
      canvasEl?.setPointerCapture?.(event.pointerId);
      touch = {
        id: event.pointerId,
        x: event.clientX,
        y: event.clientY,
        lastX: event.clientX,
        lastY: event.clientY,
        moved: false,
      };
      return;
    }
    canvasEl?.setPointerCapture?.(event.pointerId);
    // **A mouse's press keeps the keyboard on the page**: the click moves
    // focus to the body, and a page with no shell has no native view holding
    // the keyboard for it.
    if (holdsControl()) takeKeyboard();
    send('press', event);
  }

  function move(event: PointerEvent): void {
    if (event.pointerType === 'touch') {
      if (touch === null || touch.id !== event.pointerId || !holdsControl()) return;
      // **Both axes count.** A horizontal drag measured only on y never
      // became a scroll, so the lift sent a click at the end of it.
      const liftedX = Math.abs(event.clientX - touch.x);
      const liftedY = Math.abs(event.clientY - touch.y);
      if (!touch.moved && (liftedX > 6 || liftedY > 6)) touch.moved = true;
      if (touch.moved) {
        // **A finger's drag is a scroll, not a mouse drag.** A page scrolls
        // under a swipe; sending it as a held mouse button selected text and
        // dragged links instead.
        const shown = picture();
        if (shown === null) return;
        const { box, page } = shown;
        const { x, y } = pagePoint(event.clientX, event.clientY, box, page);
        // **A finger's displacement is the page's own too.** A scaled picture
        // - another surface's layout, or an agent's resize under the
        // controller - moves the page by its CSS pixels: a 39 px swipe over a
        // 390 px picture of a 1280 px page scrolls 128, not 39.
        const scaleX = box.width > 0 ? page.width / box.width : 1;
        const scaleY = box.height > 0 ? page.height / box.height : 1;
        // **The wire's deltas are the wheel's own**, so each is inverted
        // from the finger's motion: a finger moving up scrolls down, and a
        // finger moving left scrolls right.
        const dx = (event.clientX - touch.lastX) * scaleX;
        const dy = (event.clientY - touch.lastY) * scaleY;
        connection.browserInput({
          kind: 'wheel',
          x,
          y,
          delta_x: -dx,
          delta_y: -dy,
          modifiers: modifiers(event),
        });
        touch.lastX = event.clientX;
        touch.lastY = event.clientY;
      }
      return;
    }
    // **A hover is the page's too**: CSS `:hover`, menus and tooltips all
    // follow a pointer with no button held, so every move crosses rather
    // than only a drag.
    send('move', event);
  }

  function release(event: PointerEvent): void {
    if (event.pointerType === 'touch') {
      if (touch === null || touch.id !== event.pointerId) return;
      const tapped = !touch.moved;
      touch = null;
      if (tapped && holdsControl()) {
        // **The tap is also how the keyboard gets the field**: a phone opens
        // its keyboard on a real control's focus, and the canvas is not one.
        // A swipe does not focus it - the keyboard's resize would cancel the
        // swipe it interrupted.
        takeKeyboard();
        // A tap is the page's click: one press and its release at the point.
        const { x, y } = point(event);
        const gesture: BrowserInputEvent = {
          kind: 'mouse',
          action: 'press',
          x,
          y,
          button: 0,
          clicks: 1,
          modifiers: modifiers(event),
        };
        connection.browserInput(gesture);
        connection.browserInput({ ...gesture, action: 'release' });
      }
      return;
    }
    send('release', event);
  }

  /**
   * **A cancelled touch is dropped, never turned into a click.** The system
   * cancels a gesture for an edge-back, the notification shade, a
   * backgrounding or the keyboard sliding up; running the tap path there
   * activated whatever was under the finger. A mouse cancellation still owes
   * the page its release.
   */
  function cancel(event: PointerEvent): void {
    if (event.pointerType === 'touch') {
      if (touch !== null && touch.id === event.pointerId) touch = null;
      return;
    }
    send('release', event);
  }

  /**
   * **The phone's keyboard types into the page.** The canvas has no fields of
   * its own, so a hidden field takes the keyboard and the IME; printable text
   * crosses whole, and the named keys (Enter, Backspace, Tab, the arrows)
   * cross as key events. **A tap on the page focuses it**; taking control
   * alone does not, so a swipe is never interrupted by the keyboard's
   * resize, and a watcher never gets a keyboard over a page it cannot drive.
   */
  function keydown(event: KeyboardEvent): void {
    if (!holdsControl() || event.isComposing) return;
    const named = event.key.length > 1 || event.ctrlKey || event.metaKey || event.altKey;
    if (!named) return;
    if (['Shift', 'Control', 'Alt', 'Meta', 'CapsLock'].includes(event.key)) return;
    const gesture: BrowserInputEvent = {
      kind: 'key',
      down: true,
      key: event.key,
      code: event.code,
      // **Chromium edits named keys by their virtual key code**: Backspace
      // and Enter with a zero code move nothing on the page.
      windows_vk: event.keyCode,
      modifiers: modifiers(event),
    };
    connection.browserInput(gesture);
    connection.browserInput({ ...gesture, down: false });
    if (
      event.key === 'Enter' ||
      event.key === 'Backspace' ||
      event.key === 'Tab' ||
      event.key === 'Escape' ||
      event.key.startsWith('Arrow')
    ) {
      event.preventDefault();
    }
  }

  function beforeinput(event: InputEvent): void {
    if (!holdsControl() || event.isComposing) return;
    if (event.inputType === 'insertText' && typeof event.data === 'string' && event.data !== '') {
      connection.browserInput({ kind: 'text', text: event.data });
      event.preventDefault();
    }
  }

  /**
   * **A keyboard that inserts without a `beforeinput` still types.** Some
   * WebView IME paths set the field's value directly (and so does Chromium's
   * own `Input.insertText`); the value that landed is the text the page is
   * owed, and it is cleared so the next entry does not append to it.
   */
  function typed(event: Event): void {
    if (!holdsControl()) return;
    if (event instanceof InputEvent && event.isComposing) return;
    const value = imeEl?.value ?? '';
    if (value === '') return;
    if (composedText !== null && value === composedText) {
      // The composition's own commit, re-fired as an input: already sent.
      composedText = null;
      if (imeEl !== null) imeEl.value = '';
      return;
    }
    composedText = null;
    connection.browserInput({ kind: 'text', text: value });
    if (imeEl !== null) imeEl.value = '';
  }

  function composed(event: CompositionEvent): void {
    const data = event.data;
    if (holdsControl() && data !== '') {
      connection.browserInput({ kind: 'text', text: data });
      composedText = data;
      // **A re-fired input lands in this turn**; clearing on the next task
      // keeps a later identical insert from being swallowed.
      setTimeout(() => {
        if (composedText === data) composedText = null;
      }, 0);
    }
    // The committed text belongs to the page now; the field must not keep a
    // copy that the next composition would append to.
    if (imeEl !== null) imeEl.value = '';
  }

  // **A watcher never keeps the keyboard.** A surface that loses control
  // gives the field up. **A mouse that takes control can type at once**; a
  // finger's keyboard stays a tap away, because raising it on the role would
  // interrupt the swipe that came with it.
  $effect(() => {
    if (!painting) return;
    if (!holdsControl()) {
      imeEl?.blur();
      return;
    }
    if (typeof matchMedia !== 'undefined' && matchMedia('(pointer: fine)').matches) {
      imeEl?.focus({ preventScroll: true });
    }
  });

  function wheel(event: WheelEvent): void {
    event.preventDefault();
    if (!holdsControl()) return;
    const { x, y } = point(event);
    // **The deltas are a scroll amount in the page's CSS pixels, not a
    // displacement on the picture**, so only the point is fitted: a wheel
    // tick scrolls the page as far whatever size the picture is drawn at.
    const gesture: BrowserInputEvent = {
      kind: 'wheel',
      x,
      y,
      delta_x: -event.deltaX,
      delta_y: -event.deltaY,
      modifiers: modifiers(event),
    };
    connection.browserInput(gesture);
  }

  function measure(): void {
    if (!pageEl) return;
    const box = pageEl.getBoundingClientRect();
    setRegion(
      { x: box.x, y: box.y, width: box.width, height: box.height },
      { width: window.innerWidth, height: window.innerHeight },
      window.devicePixelRatio || 1,
    );
  }

  /**
   * **A new open is a new surface identity.** The store mints a generation
   * per Open and drops a superseded answer; the mount of a fresh takeover
   * reports its page area once, even when the layout it measures is
   * unchanged (a retry, or a reopen at the same size).
   */
  $effect(() => {
    // **The bar's height moves the page area**: a row that grows over the
    // picture (the control changing hands) must be re-measured, or the
    // overlay keeps the old top and draws over the bar.
    void surface.barHeight;
    measure();
  });

  /**
   * **The measurement is taken again once the window has settled.** A native
   * resize moves the webview's frame, and the page's own re-measure can land
   * before AppKit has; the shell asks again after it settles, because a
   * transient that moved the box without resizing it fires no ResizeObserver.
   */
  function remeasure(): void {
    if (typeof requestAnimationFrame === 'undefined') {
      measure();
      return;
    }
    requestAnimationFrame(measure);
  }

  onMount(() => {
    const observer = typeof ResizeObserver === 'undefined' ? null : new ResizeObserver(measure);
    observer?.observe(pageEl);
    window.addEventListener('resize', measure);
    window.addEventListener('forge-surface-remeasure', remeasure);
    return () => {
      observer?.disconnect();
      window.removeEventListener('resize', measure);
      window.removeEventListener('forge-surface-remeasure', remeasure);
    };
  });

  onMount(() => {
    // **The takeover takes the keyboard with it.** A composer focused on the
    // page behind keeps the software keyboard up in the WebView, and it
    // would sit over the page the person just opened.
    if (typeof document !== 'undefined' && document.activeElement instanceof HTMLElement) {
      document.activeElement.blur();
    }
    // **The shell's takeover is the store's**: the open and the lower both go
    // through it, so full screen and the viewer follow one door each. Leaving
    // the page is the only case the store does not see, and this closes it -
    // **unless a newer MOUNT has taken the surface since**, which this
    // component's teardown must not close under it. A retry or a pick is not
    // a new mount and leaves this token current.
    const mine = claimSurfaceMount();
    return () => {
      if (surfaceMountIsCurrent(mine) && surface.phase !== 'closed') void closeSurface();
    };
  });
</script>

<div class="takeover" data-phase={surface.phase} style:top={`${surface.barHeight}px`}>
  <div class="takeover-page" bind:this={pageEl} role="region" aria-label="the app's browser">
    {#if surface.phase === 'opening'}
      <p class="surface-tx">opening the browser…</p>
    {:else if surface.phase === 'failed'}
      <p class="surface-tx bad">{surface.reason}</p>
    {:else if painting}
      <canvas
        class="surface-canvas"
        bind:this={canvasEl}
        aria-label="the browser's page"
        onpointerdown={press}
        onpointermove={move}
        onpointerup={release}
        onpointercancel={cancel}
        onwheel={wheel}
      ></canvas>
      <!-- The phone's keyboard and IME: invisible, but a tap on the page
           focuses it, so its keys and composed text reach the page through
           the same input vocabulary the desktop overlay uses. -->
      <input
        class="surface-ime"
        bind:this={imeEl}
        type="text"
        data-editor="nowhere"
        autocomplete="off"
        autocapitalize="off"
        spellcheck="false"
        aria-hidden="true"
        tabindex="-1"
        onbeforeinput={beforeinput}
        oninput={typed}
        oncompositionend={composed}
        onkeydown={keydown}
      />
    {/if}
  </div>
</div>
