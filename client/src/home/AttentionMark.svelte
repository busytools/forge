<script lang="ts">
  import { portal } from '../chat/strip-panel';
  import { hrefFor } from '../routes';
  import type { AttentionRow } from '../wire/home';
  import { elapsedLabel } from './view';

  /**
   * The decision-wait mark: a glyph and a count, with the asks themselves
   * behind it - each one opening the project's board, where its row waits.
   *
   * **The count is what a row already knows, and the list is why it is
   * there.** A wait lives on the board, which may not be open, so the rail
   * and the home are the surfaces that make it visible; the popover says
   * which ask it is, and its item is the way to answer it.
   *
   * The panel is portaled (`use:portal`) and positioned in viewport terms,
   * because the rail's own scroller would clip a panel drawn inside it.
   */
  let {
    waits,
    org,
    project,
    now,
  }: {
    /** This row's waits: the project's on its own row, a seat's on its chip. */
    waits: AttentionRow[];
    org: string;
    project: string;
    /** The page's clock, for the age of each ask. */
    now: number;
  } = $props();

  /** The panel is portaled, so the button names it rather than preceding it. */
  const panelId = $props.id();

  let open = $state(false);
  let toggleEl: HTMLElement | null = $state(null);
  let panelEl: HTMLElement | null = $state(null);
  /** The panel's measured place, remeasured while open as its anchor moves. */
  let limits = $state('');

  const count = $derived(waits.length);
  const words = $derived(count === 1 ? '1 decision wait on you' : `${count} decision waits on you`);
  const title = $derived.by(() => {
    const [first] = waits;
    return first === undefined ? words : `${words}: ${first.words}`;
  });

  /**
   * The panel below its mark, clamped to the window. **Viewport terms, not
   * mark terms**: the panel is portaled to the body, so nothing here may
   * resolve against the row it hangs from.
   */
  function panelStyle(el: HTMLElement | null): string {
    if (el === null) return '';
    const box = el.getBoundingClientRect();
    const width = Math.round(Math.min(340, window.innerWidth - 16));
    const left = Math.round(Math.max(8, Math.min(box.left, window.innerWidth - width - 8)));
    const top = Math.round(box.bottom + 6);
    const room = Math.round(Math.max(120, window.innerHeight - top - 8));
    return `left:${left}px;top:${top}px;width:${width}px;max-height:${Math.min(320, room)}px`;
  }

  $effect(() => {
    if (!open) return;
    // A row's mark moves under the panel - the rail scrolls, a live count
    // re-wraps - so the place is measured each frame while it is open.
    let raf = requestAnimationFrame(function tick() {
      limits = panelStyle(toggleEl);
      raf = requestAnimationFrame(tick);
    });
    return () => cancelAnimationFrame(raf);
  });

  /** A press outside the mark and its panel closes it. */
  $effect(() => {
    if (!open) return;
    const down = (event: PointerEvent): void => {
      const path = event.composedPath();
      if (toggleEl !== null && path.includes(toggleEl)) return;
      if (panelEl !== null && path.includes(panelEl)) return;
      open = false;
    };
    const key = (event: KeyboardEvent): void => {
      if (event.key !== 'Escape') return;
      event.preventDefault();
      open = false;
      backToToggle();
    };
    document.addEventListener('pointerdown', down, true);
    document.addEventListener('keydown', key);
    return () => {
      document.removeEventListener('pointerdown', down, true);
      document.removeEventListener('keydown', key);
    };
  });

  /** Put focus back on the mark, which is where a dismissal leaves a reader. */
  function backToToggle(): void {
    if (toggleEl !== null) toggleEl.focus();
  }

  function toggle(): void {
    if (open) {
      open = false;
      return;
    }
    limits = panelStyle(toggleEl);
    open = true;
  }
</script>

{#if count > 0}
  <button
    type="button"
    class="attn"
    bind:this={toggleEl}
    aria-expanded={open}
    aria-controls={open ? panelId : undefined}
    aria-label={words}
    {title}
    onclick={toggle}
  >
    <span class="attn-glyph" aria-hidden="true"></span>
    <span class="attn-n">{count}</span>
  </button>
  {#if open}
    <div class="attn-pop" id={panelId} bind:this={panelEl} style={limits} use:portal>
      <ul class="attn-list">
        {#each waits as wait (wait.task)}
          <li>
            <a
              class="attn-wait"
              href={hrefFor({ name: 'board', org, project })}
              onclick={() => (open = false)}
            >
              <span class="attn-who">
                <span class="attn-seat">{wait.owner}</span>
                <span class="attn-age">{elapsedLabel(wait.at, now)}</span>
              </span>
              <span class="attn-subject">{wait.subject}</span>
              <span class="attn-words">{wait.words}</span>
            </a>
          </li>
        {/each}
      </ul>
    </div>
  {/if}
{/if}
