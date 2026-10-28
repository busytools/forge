<script lang="ts">
  import Icon from '../components/Icon.svelte';
  import { tabRows, whyText, type TabRow } from '../browser/host';
  import { canAttach, listTabs, onConnection, onTabs } from '../browser/link';
  import { closeSurface, openSurface, surface } from '../browser/surface.svelte';
  import type { BrowserTabList } from '../protocol';
  import { panelStyle, portal } from './strip-panel';
  import type { SessionSlot } from '../wire/types';

  /**
   * The browser row in the strip above the composer: the resolved profile's
   * tabs, read from the SERVER.
   *
   * **The browser is the server's, and its tabs are the sessions'** - one tab
   * per session's calls, each owned by the seat driving it. The row says how
   * the profile is doing (a host, a driver, the idle deadline) and, opened,
   * lists the tabs: their number, title, owner and state. A tab's own **show**
   * raises the takeover on that tab; there is no profile to close or delete
   * from here, and no tab to create - a tab belongs to the calls that drive
   * it.
   */
  let { slot = null }: { slot?: SessionSlot | null } = $props();

  /** Whether this page can show the server's browser at all. */
  const capable = canAttach();

  let open = $state(false);
  /** The last list the shell answered, and the rows it draws. */
  let listing = $state<BrowserTabList | null>(null);
  let rows = $state<TabRow[]>([]);
  /**
   * What the last read answered. "No tabs yet" is a claim about the server's
   * state, and a read that never answered has no state to claim - so the row
   * waits, or says why, rather than asserting one.
   */
  let read = $state<'loading' | 'ready' | 'failed'>('loading');
  let why = $state<string | null>(null);

  /** The clock the idle line reads: a minute's tick is all "closing in Nm" needs. */
  let now = $state(Date.now());

  $effect(() => {
    const timer = setInterval(() => {
      now = Date.now();
    }, 30_000);
    return () => clearInterval(timer);
  });

  /** One tab row's state, as its own mark: a call, a person, or neither. */
  function mark(row: TabRow): 'ring' | 'idle' | 'held' {
    return row.state === 'in_use' ? 'ring' : row.state === 'held' ? 'held' : 'idle';
  }

  /** How the profile's connection reads in one line. */
  const connectedText = $derived(
    listing === null
      ? 'reading'
      : listing.up
        ? listing.connected
          ? 'connected'
          : 'the host is up, no driver attached'
        : 'no host up',
  );

  /** When the idle clock closes the profile, in the row's own words. */
  const closingText = $derived.by((): string | null => {
    const at = listing?.closing_at_ms;
    if (at === null || at === undefined) return null;
    const minutes = Math.max(0, Math.ceil((at - now) / 60_000));
    return minutes === 0 ? 'closing now' : `closing in ${minutes}m`;
  });

  /** The hover a mouse gets: whose profile this is, and how it stands. */
  const hover = $derived(
    listing === null
      ? "the server's browser"
      : `${listing.profile} · ${connectedText}${closingText === null ? '' : ` · ${closingText}`}`,
  );

  /**
   * A count is a claim about a read, so before one has answered - or when the
   * last one failed and nothing was ever read - the row says the count is not
   * known rather than standing on a zero nothing measured.
   */
  const count = $derived(
    read === 'ready'
      ? `${rows.length} tab${rows.length === 1 ? '' : 's'}`
      : read === 'loading'
        ? '…'
        : 'count unknown',
  );

  /**
   * Whether THIS client is showing `number` of the listed profile: the tab
   * state is the server's, but which tab this page paints is its own.
   */
  function fronting(number: number): boolean {
    return (
      surface.phase !== 'closed' &&
      listing !== null &&
      surface.attachedProfile === listing.profile &&
      surface.tab === number
    );
  }

  /** One list into the panel; a row this page cannot read fails the read. */
  function absorb(list: BrowserTabList): void {
    listing = list;
    try {
      rows = tabRows(list);
      read = 'ready';
      why = null;
    } catch (error) {
      read = 'failed';
      why = whyText(error);
    }
  }

  /**
   * What each read was issued as, so a read that lands after a newer one -
   * or after a failed raise - cannot write over what came later.
   */
  let readToken = 0;

  /** The profile's tabs, read at mount and when the panel opens. */
  async function readTabs(): Promise<void> {
    const token = (readToken += 1);
    try {
      const list = await listTabs(slot);
      if (token !== readToken) return;
      absorb(list);
    } catch (error) {
      if (token !== readToken) return;
      read = 'failed';
      why = whyText(error);
    }
  }

  $effect(() => {
    void readTabs();
  });

  // **The ask subscribes the connection; the push follows it.** The shell
  // emits every change as `browser_tabs`, so the panel never polls.
  $effect(() => {
    let stop: (() => void) | null = null;
    let live = true;
    void onTabs((list) => {
      if (live) absorb(list);
    }).then((off) => {
      if (live) stop = off;
      else off();
    });
    return () => {
      live = false;
      stop?.();
    };
  });

  /**
   * **A reconnect re-reads the tabs.** The push subscription lived on the
   * socket that dropped; without a read the strip froze on its last list
   * while a fresh connection pushed nothing.
   */
  $effect(() => {
    let stop: (() => void) | null = null;
    let live = true;
    void onConnection(() => {
      if (live) void readTabs();
    }).then((off) => {
      if (live) stop = off;
      else off();
    });
    return () => {
      live = false;
      stop?.();
    };
  });

  /**
   * The takeover moving - the hardware Back on the phone, or a raise - says
   * nothing in THIS page acted, so every word re-reads the rows; a read made
   * by a caller can land before the takeover did.
   */
  $effect(() => {
    const onTakeover = (event: Event): void => {
      const what = (event as CustomEvent<string>).detail;
      if (what !== 'lowered' && what !== 'done' && what !== 'raised') return;
      void readTabs();
    };
    window.addEventListener('forge-takeover', onTakeover);
    return () => window.removeEventListener('forge-takeover', onTakeover);
  });

  /** Show one tab, or lower the one this page already has up. */
  function show(number: number): void {
    if (listing === null) return;
    if (fronting(number)) {
      void closeSurface(listing.profile, slot).then(() => void readTabs());
      return;
    }
    void openSurface(listing.profile, slot, number).then((reason) => {
      if (reason !== null) {
        // A read in flight was issued before this failure and must not erase
        // it when it lands.
        readToken += 1;
        read = 'failed';
        why = reason;
        return;
      }
      void readTabs();
    });
  }

  let segEl = $state<HTMLElement | null>(null);
  /** The panel, portaled to the body, so the leave guards can see it. */
  let panelEl = $state<HTMLElement | null>(null);
  /** The panel's measured caps, remeasured every frame while open. */
  let limits = $state('');

  $effect(() => {
    if (!open) return;
    // **While the panel is open it follows the layout it hangs in.** The
    // style is measured once at the open, and an ordinary reflow would
    // otherwise leave the panel where the old geometry put it.
    let raf = requestAnimationFrame(function tick() {
      limits = panelStyle(segEl);
      raf = requestAnimationFrame(tick);
    });
    return () => cancelAnimationFrame(raf);
  });

  /**
   * How the panel closes when the pointer leaves: with a grace period, because
   * a gap sits between the toggle and the panel and a pointer crossing it
   * would otherwise never reach the rows.
   */
  let leaving: ReturnType<typeof setTimeout> | null = null;

  /** Open, and read the tabs fresh, which is what every opening does. */
  function hold(): void {
    if (leaving !== null) clearTimeout(leaving);
    leaving = null;
    open = true;
    void readTabs();
  }

  function shut(): void {
    open = false;
  }

  function arm(): void {
    if (leaving !== null) clearTimeout(leaving);
    leaving = setTimeout(() => {
      leaving = null;
      shut();
    }, 120);
  }

  /**
   * Leaving arms the close, with the guards every sibling row carries: a
   * pointer leave arriving over the panel's own gap is not a departure while
   * the segment is still hovered, and a leave may close while the TOGGLE
   * holds focus but not while a panel control does.
   */
  function release(event?: FocusEvent): void {
    if (event === undefined) {
      if (segEl !== null && segEl.matches(':hover')) return;
      if (panelEl !== null && panelEl.matches(':hover')) return;
      const active = document.activeElement;
      const toggle = segEl?.querySelector('button') ?? null;
      if (
        active instanceof Node &&
        ((segEl !== null && segEl.contains(active)) ||
          (panelEl !== null && panelEl.contains(active))) &&
        active !== toggle
      ) {
        return;
      }
      arm();
      return;
    }
    const next = event.relatedTarget ?? document.activeElement;
    if (
      next instanceof Node &&
      ((segEl !== null && segEl.contains(next)) || (panelEl !== null && panelEl.contains(next)))
    ) {
      return;
    }
    arm();
  }

  /** A pointer that can hover, which a finger cannot. */
  const hovering = (event: PointerEvent) => event.pointerType === 'mouse';

  /**
   * A compatibility press is in flight: **a tap focuses the toggle through
   * its compat `mousedown`**, so focus opens the panel only while a press is
   * in flight; `mouseup` ends every quiet press.
   */
  let pointed = false;

  $effect(() => {
    const press = () => (pointed = true);
    const settle = () => (pointed = false);
    window.addEventListener('mousedown', press);
    window.addEventListener('mouseup', settle);
    window.addEventListener('click', settle);
    window.addEventListener('pointercancel', settle);
    return () => {
      window.removeEventListener('mousedown', press);
      window.removeEventListener('mouseup', settle);
      window.removeEventListener('click', settle);
      window.removeEventListener('pointercancel', settle);
    };
  });

  /** The toggle took focus: opening from it, unless a press put it there. */
  function focusIn(): void {
    if (pointed) {
      pointed = false;
      return;
    }
    hold();
  }

  /** Put focus back on the toggle, which is where a dismissal leaves a reader. */
  function toToggle(): void {
    segEl?.querySelector('button')?.focus();
  }

  // A close armed when the segment unmounts would write to a dead instance.
  $effect(() => () => {
    if (leaving !== null) clearTimeout(leaving);
  });

  // A pointer landing outside the segment closes the list. **The panel counts
  // as inside**: it is PORTALED to the body, so it is not a descendant.
  $effect(() => {
    if (!open) return;
    const away = (event: PointerEvent): void => {
      const at = event.target;
      if (
        at instanceof Node &&
        ((segEl !== null && segEl.contains(at)) || (panelEl !== null && panelEl.contains(at)))
      ) {
        return;
      }
      shut();
    };
    document.addEventListener('pointerdown', away);
    return () => document.removeEventListener('pointerdown', away);
  });

  /** The toggle collapses, like every sibling row: touch has no hover, so this
   * is both its open and its close. */
  function toggle(): void {
    if (open) {
      if (leaving !== null) clearTimeout(leaving);
      leaving = null;
      shut();
      return;
    }
    hold();
  }

  /** Escape closes, and focus goes back to the toggle first. */
  function esc(event: KeyboardEvent): void {
    if (event.key !== 'Escape') return;
    toToggle();
    if (leaving !== null) clearTimeout(leaving);
    leaving = null;
    shut();
  }
  /**
   * **The row is only there when there is something behind it.** A profile
   * with no tab is a browser nothing has invoked yet, and an empty row saying
   * "0 tabs" is noise on every page that has never used one - so it draws
   * nothing at all. The list's own pushes decide it, so the row appears when
   * the first tab does and leaves when the last one closes.
   */
  const present = $derived(listing !== null && listing.rows.length > 0);
</script>

{#if present}
  <span class="bz-seg" class:open bind:this={segEl}>
    <button
      type="button"
      class="bz-tog"
      aria-expanded={open}
      aria-label="the browser's tabs"
      title={hover}
      onclick={toggle}
      onpointerenter={(event) => {
        if (hovering(event)) hold();
      }}
      onpointerleave={(event) => {
        if (hovering(event)) release();
      }}
      onfocusin={focusIn}
      onfocusout={release}
      onkeydown={esc}
    >
      <Icon name="web" />
      {#if listing?.up && listing.connected}
        <span class="dot ok" title="the browser is connected"></span>
      {:else if listing?.up}
        <span class="dot idle" title="the browser host is up"></span>
      {:else if capable && listing !== null}
        <Icon name="x" class="bad" />
      {/if}
      browser
      <span class="n">{count}</span>
    </button>

    {#if open}
      <div
        class="bz-list"
        style={limits}
        bind:this={panelEl}
        use:portal
        role="group"
        aria-label="the browser's tabs"
        onpointerenter={(event) => {
          if (hovering(event)) hold();
        }}
        onpointerleave={(event) => {
          if (hovering(event)) release();
        }}
        onfocusout={release}
      >
        <div class="bz-state">
          <span class="tx">
            {listing === null ? 'reading the browser' : `${listing.profile} · ${connectedText}`}
          </span>
          <span class="tx bz-engine">
            {closingText ?? (listing?.up ? "the server's own Chromium" : 'nothing to run a page')}
          </span>
        </div>

        {#each rows as row (row.number)}
          <div class="bz-it">
            {#if mark(row) === 'ring'}
              <span class="ring" title="a call is driving this tab"></span>
            {:else if mark(row) === 'held'}
              <span class="dot warn" title="a hand-off is waiting here"></span>
            {:else}
              <span class="dot idle" title="this tab is idle"></span>
            {/if}
            <span class="nm">tab {row.number}</span>
            <span class="tx" title={row.url}>{row.title}</span>
            <span class="tx">{row.owner} · {row.stateText}</span>
            {#if capable}
              <button
                type="button"
                class="bz-take bz-show"
                aria-label={fronting(row.number)
                  ? `hide tab ${row.number}`
                  : `show tab ${row.number} of ${listing?.profile ?? 'the browser'}`}
                onclick={() => show(row.number)}
                onkeydown={esc}
              >
                {fronting(row.number) ? 'hide' : 'show'}
              </button>
            {/if}
          </div>
        {/each}

        {#if !capable}
          <div class="bz-it">
            <Icon name="x" class="bad" />
            <span class="tx">this client cannot show the browser</span>
          </div>
        {:else if read === 'loading' && listing === null}
          <div class="bz-it"><span class="tx">reading the browser's tabs…</span></div>
        {:else if read === 'failed'}
          <div class="bz-it"><span class="tx bad">{why}</span></div>
        {:else if rows.length === 0}
          <div class="bz-it"><span class="tx">no tabs yet</span></div>
        {/if}
      </div>
    {/if}
  </span>
{/if}
