<script lang="ts">
  import { untrack } from 'svelte';

  import BrowserSegment from '../chat/BrowserSegment.svelte';
  import ConnectorsSegment from '../chat/ConnectorsSegment.svelte';
  import FilesSegment from '../chat/FilesSegment.svelte';
  import GitSegment from '../chat/GitSegment.svelte';
  import McpSegment from '../chat/McpSegment.svelte';
  import MonitorsSegment from '../chat/MonitorsSegment.svelte';
  import ProcessesSegment from '../chat/ProcessesSegment.svelte';
  import ReviewSegment from '../chat/ReviewSegment.svelte';
  import SchedulesSegment from '../chat/SchedulesSegment.svelte';
  import Strip from '../chat/Strip.svelte';
  import SubagentSegment from '../chat/SubagentSegment.svelte';
  import TasksSegment from '../chat/TasksSegment.svelte';
  import { connectors } from '../chat/connectors.svelte';
  import { files } from '../chat/files.svelte';
  import { git } from '../chat/git.svelte';
  import { mcp } from '../chat/mcp.svelte';
  import { monitors } from '../chat/monitors.svelte';
  import { processes } from '../chat/processes.svelte';
  import { review } from '../chat/review.svelte';
  import { schedules } from '../chat/schedules.svelte';
  import { panelStyle, portal } from '../chat/strip-panel';
  import { subagents } from '../chat/subagents.svelte';
  import { tasks } from '../chat/tasks.svelte';
  import { hrefFor } from '../routes';
  import type { TurnInfo } from '../chat/units';
  import type { SessionSlot } from '../wire/types';

  /**
   * The live row: what this seat is doing, on one line, at the top of the bar.
   *
   * **The strip outlives the turn, and so does this row.** It used to draw only
   * while a turn was being written, which left an idle seat with no way at all
   * into its tree, its tasks or its watchers; the segments are the home for
   * what the inspector used to hold. So the row stands whenever it has anything
   * to say, the running turn being one of the things it may say.
   *
   * **It mounts the segments rather than re-drawing them.** Every chip is the
   * same component the strip draws, in the strip's fixed order, so a segment
   * gaining a state gains it here for free - and a row that re-implemented one
   * would be the second copy of it.
   *
   * **It never wraps.** The chips that do not fit are collapsed behind `+N`,
   * which lists them in a panel; the ragged wrap this replaced grew the band to
   * three rows between 780 and 860px (Ved, in the #2159 look).
   */

  let {
    info,
    slot = null,
  }: {
    info: TurnInfo | null;
    slot?: SessionSlot | null;
  } = $props();

  /** Whether any segment has anything to state, which is the row's own gate. */
  const rows = $derived(
    subagents.all().length > 0 ||
      files.anything() ||
      processes.anything() ||
      connectors.anything() ||
      schedules.anything() ||
      mcp.anything() ||
      git.anything() ||
      tasks.anything() ||
      review.anything() ||
      monitors.anything(),
  );

  /** The row's own element, which is what its width is measured on. */
  let rowEl: HTMLElement | null = $state(null);
  /** The `+N` button, which the panel hangs off and the measurement reserves. */
  let moreEl: HTMLButtonElement | null = $state(null);
  /** The chips that do not fit, in the row's order: empty while they all do. */
  let hidden = $state<string[]>([]);
  /** Whether the panel lists the hidden chips. */
  let open = $state(false);
  /** The panel's measured caps, remeasured every frame while open. */
  let limits = $state('');
  let panelEl: HTMLElement | null = $state(null);

  /** The gap the sheet draws when no layout has computed one (jsdom, a row not yet painted). */
  const GAP_FLOOR = 2;

  /**
   * Which chips fit, measured from the row and the chips themselves.
   *
   * **The measurement reads the live layout, not a guess**: the turn figures
   * and divider lead, then each chip that is actually drawn adds its own width,
   * and the last chip that still leaves room for the `+N` stays on the row. A
   * chip whose store says nothing draws no element, so it costs nothing here.
   *
   * **With no layout there is no answer**, so every chip stays on the row:
   * a zero width is a row that has not been painted yet, not a row that cannot
   * hold one chip.
   */
  function measure(): void {
    const row = rowEl;
    if (row === null) return;
    const width = row.clientWidth;
    if (width <= 0) {
      if (hidden.length > 0) hidden = [];
      return;
    }
    const style = getComputedStyle(row);
    const pad =
      (Number.parseFloat(style.paddingLeft) || 0) + (Number.parseFloat(style.paddingRight) || 0);
    const limit = width - pad;
    const track = row.querySelector<HTMLElement>('.live-segs');
    const gap =
      Number.parseFloat(track === null ? '' : getComputedStyle(track).columnGap) || GAP_FLOOR;
    const lead = [
      row.querySelector<HTMLElement>('[data-fig]'),
      row.querySelector<HTMLElement>('[data-div]'),
    ];
    let used = 0;
    for (const el of lead) if (el !== null) used += el.offsetWidth + gap;
    const chips = [...row.querySelectorAll<HTMLElement>('[data-chip]')].filter(
      (el) => el.firstElementChild !== null,
    );
    let all = used;
    for (const el of chips) all += el.offsetWidth + gap;
    if (all <= limit) {
      if (hidden.length > 0) hidden = [];
      return;
    }
    const reserve = (moreEl?.offsetWidth ?? 0) + gap;
    const names: string[] = [];
    for (const el of chips) {
      if (names.length === 0 && used + el.offsetWidth + gap + reserve <= limit) {
        used += el.offsetWidth + gap;
        continue;
      }
      names.push(el.dataset.chip ?? '');
    }
    const same = names.length === hidden.length && names.every((name, at) => name === hidden[at]);
    if (!same) hidden = names;
  }

  /**
   * Watch the row, every chip, and the window, and measure on each change.
   *
   * **The row's own size is not the whole trigger**: a chip's label growing as
   * its counts tick moves the fit without moving the row, so every chip is
   * watched too. `untrack` keeps the measurement out of the effect's own
   * dependencies - it writes the hidden set, and an effect that reads what it
   * writes is an update loop.
   */
  function watch(row: HTMLElement): () => void {
    const run = () => untrack(measure);
    run();
    const watching = typeof ResizeObserver === 'undefined' ? null : new ResizeObserver(run);
    watching?.observe(row);
    for (const el of row.querySelectorAll('[data-chip]')) watching?.observe(el);
    window.addEventListener('resize', run);
    return () => {
      watching?.disconnect();
      window.removeEventListener('resize', run);
    };
  }

  $effect(() => {
    const row = rowEl;
    void rows;
    if (row === null) return;
    return watch(row);
  });

  /** The panel goes with the reason it existed. */
  $effect(() => {
    if (hidden.length === 0) open = false;
  });

  /** The panel's caps, remeasured every frame while open: the row moves under it. */
  $effect(() => {
    if (!open) return;
    let raf = requestAnimationFrame(function tick() {
      limits = panelStyle(moreEl);
      raf = requestAnimationFrame(tick);
    });
    return () => cancelAnimationFrame(raf);
  });

  /** A pointer landing outside the button and its panel closes it. */
  function away(event: Event): void {
    const at = event.target;
    if (!(at instanceof Node)) return;
    if (moreEl?.contains(at) === true || panelEl?.contains(at) === true) return;
    open = false;
  }

  $effect(() => {
    if (!open) return;
    document.addEventListener('pointerdown', away);
    return () => document.removeEventListener('pointerdown', away);
  });

  /** Escape closes the panel from anywhere inside it, not only from the button. */
  $effect(() => {
    if (!open) return;
    const key = (event: KeyboardEvent) => {
      if (event.key === 'Escape') close();
    };
    document.addEventListener('keydown', key);
    return () => document.removeEventListener('keydown', key);
  });

  function toggle(): void {
    if (open) {
      close();
      return;
    }
    if (hidden.length === 0) return;
    limits = panelStyle(moreEl);
    open = true;
  }

  /** Give the keyboard back to the button that opened the panel. */
  function close(): void {
    open = false;
    moreEl?.focus();
  }
</script>

{#snippet segment(name: string)}
  {#if name === 'browser'}
    <BrowserSegment {slot} />
  {:else if name === 'subagents'}
    <SubagentSegment />
  {:else if name === 'processes'}
    <ProcessesSegment />
  {:else if name === 'connectors'}
    <ConnectorsSegment />
  {:else if name === 'schedules'}
    <SchedulesSegment />
  {:else if name === 'mcp'}
    <McpSegment />
  {:else if name === 'files'}
    <FilesSegment href={slot === null ? null : hrefFor({ name: 'files', slot })} />
  {:else if name === 'git'}
    <GitSegment />
  {:else if name === 'review'}
    <ReviewSegment href={slot === null ? null : hrefFor({ name: 'review', slot })} />
  {:else if name === 'tasks'}
    <TasksSegment
      href={slot === null ? null : hrefFor({ name: 'board', org: slot.org, project: slot.project })}
    />
  {:else if name === 'monitors'}
    <MonitorsSegment />
  {/if}
{/snippet}

{#if info !== null || rows}
  <div class="live-row" bind:this={rowEl}>
    {#if info !== null}
      <span class="live-fig" data-fig><Strip {info} /></span>
      <span class="live-div" data-div aria-hidden="true"></span>
    {/if}
    <!-- The chips share one list, in the strip's fixed order. Each is the
         segment component itself; the wrapper carries only what the row adds -
         the collapse, and the tone of a thing waiting behind it. -->
    <div class="live-segs" role="list" aria-label="what this seat is holding">
      <span
        class="live-chip"
        class:off={hidden.includes('browser')}
        data-chip="browser"
        role="listitem"
      >
        {@render segment('browser')}
      </span>
      <span
        class="live-chip"
        class:off={hidden.includes('subagents')}
        data-chip="subagents"
        role="listitem"
      >
        {@render segment('subagents')}
      </span>
      <!-- The processes row is the one segment whose failure is a verdict the
           row can read: a settled call the CLI marked failed. -->
      <span
        class="live-chip"
        class:off={hidden.includes('processes')}
        class:bad={processes.rows().some((row) => row.failed)}
        data-chip="processes"
        role="listitem"
      >
        {@render segment('processes')}
      </span>
      <span
        class="live-chip"
        class:off={hidden.includes('connectors')}
        data-chip="connectors"
        role="listitem"
      >
        {@render segment('connectors')}
      </span>
      <span
        class="live-chip"
        class:off={hidden.includes('schedules')}
        data-chip="schedules"
        role="listitem"
      >
        {@render segment('schedules')}
      </span>
      <span class="live-chip" class:off={hidden.includes('mcp')} data-chip="mcp" role="listitem">
        {@render segment('mcp')}
      </span>
      <span
        class="live-chip"
        class:off={hidden.includes('files')}
        data-chip="files"
        role="listitem"
      >
        {@render segment('files')}
      </span>
      <span class="live-chip" class:off={hidden.includes('git')} data-chip="git" role="listitem">
        {@render segment('git')}
      </span>
      <!-- The review answer waiting is the live fact a reviewer is looking
           for, so its chip wears the accent. -->
      <span
        class="live-chip"
        class:off={hidden.includes('review')}
        class:att={review.waiting() > 0}
        data-chip="review"
        role="listitem"
      >
        {@render segment('review')}
      </span>
      <span
        class="live-chip"
        class:off={hidden.includes('tasks')}
        data-chip="tasks"
        role="listitem"
      >
        {@render segment('tasks')}
      </span>
      <span
        class="live-chip"
        class:off={hidden.includes('monitors')}
        data-chip="monitors"
        role="listitem"
      >
        {@render segment('monitors')}
      </span>
    </div>
    <button
      class="live-more"
      class:idle={hidden.length === 0}
      bind:this={moreEl}
      type="button"
      aria-expanded={open}
      aria-label={hidden.length > 0 ? `${hidden.length} more` : undefined}
      onclick={toggle}
      onkeydown={(event) => {
        if (event.key !== 'Escape') return;
        close();
      }}
    >
      +{hidden.length}
    </button>
  </div>
{/if}

{#if open && hidden.length > 0}
  <div
    class="live-panel"
    style={limits}
    use:portal
    bind:this={panelEl}
    role="group"
    aria-label="the rest of what this seat is holding"
  >
    {#each hidden as name (name)}
      <span class="lp-seg" data-panel-chip={name}>{@render segment(name)}</span>
    {/each}
  </div>
{/if}
