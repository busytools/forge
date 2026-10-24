<script lang="ts">
  import { tick } from 'svelte';
  import { SvelteMap, SvelteSet } from 'svelte/reactivity';
  import Icon from '../components/Icon.svelte';
  import {
    canHost,
    closeProfile,
    deleteProfile,
    listProfiles,
    whyText,
    type ProfileRow,
  } from '../browser/host';
  import { closeSurface, openSurface, surface } from '../browser/surface.svelte';
  import { panelStyle, portal } from './strip-panel';
  import type { SessionSlot } from '../wire/types';

  /**
   * The browser row in the strip above the composer: how many profiles the
   * SERVER holds, and - opened - who owns each, whether a client has it up
   * as the takeover, and the controls the person has.
   *
   * **A segment beside the agents row, not a page of its own.** The inspector
   * is going, and this is the shape its survivors take: a row in the strip,
   * its list opening in place (the mockup Ved settled, 2026-10-06). When the
   * surviving strip lands, this mounts into it unchanged.
   *
   * The controls are the person's, never a session's: a profile row's
   * **close** (which ends the run and keeps the logins), its **delete**
   * (which takes them), and **show** / **hide** (the takeover).
   *
   * **The profiles are the SERVER's state** - it owns the hosts, the drivers
   * and the profile directories - so the row reads them from the server
   * through the shell, for this seat.
   */
  let { slot = null }: { slot?: SessionSlot | null } = $props();

  /** Whether this page runs in a shell that can show the server's browser. */
  const capable = canHost();

  let open = $state(false);
  /**
   * The strip's own snapshot: read at mount and when the list opens, not per
   * frame. **The shell serves it FOR A SEAT** - this page's slot - so the
   * rows are already the ones this seat may see (its own, plus the
   * ownerless legacy ones the host has not swept yet); no page-side filter
   * could hide a name the host answered.
   */
  let profiles = $state<ProfileRow[]>([]);
  /** This seat's own key, in the shell's own join, for the empty line. */
  const mine = $derived(slot === null ? null : `${slot.org}/${slot.project}/${slot.label}`);
  /**
   * The closed profiles this read answered: no live entry behind them, their
   * files - the logins - still here. **The bulk door's set**, and the page
   * takes the shell's word for what closed means rather than guessing at it
   * from the other flags.
   */
  const closed = $derived(profiles.filter((row) => row.closed));

  /**
   * A row's own second line, in the shell's states. **A closed profile says
   * closed and keeps the logins it still holds, whichever owner it has** -
   * it is the row the bulk delete takes. An entry that is held but not
   * running is NOT closed: it keeps the words its own state earns, "not
   * opened yet" or "its driver is gone", so the two never read alike.
   */
  function stateText(row: ProfileRow): string {
    if (row.closed) {
      return `${row.owner === '' ? '' : `${row.owner} · `}closed · its logins are still here`;
    }
    return `${row.owner}${row.up ? '' : ' · not running'}`;
  }
  /**
   * The row whose delete is confirming, and the row a delete is running for:
   * Shape A - the confirm happens in the row itself (Ved's pick, 2026-10-11).
   * One at a time, and the confirm clears on every door that closes the list.
   */
  let armed = $state<string | null>(null);
  /** Every row a delete is running for: deletes on different rows overlap,
   *  and one completing must not clear another's mark. */
  let deleting = new SvelteSet<string>();
  /**
   * The last delete's own outcome, drawn in place of the row that went:
   * "deleted" is a status a click owes the person, not a silence where the
   * row used to be. Cleared when the panel closes or reopens, so it cannot
   * be read as news about a later state.
   */
  let outcome = $state<string | null>(null);
  /**
   * The bulk confirm's own list, snapshotted when it armed: the list the
   * person read is the list that goes, even if the host sweeps a row out
   * from under the panel in between. Empty means nothing is confirming.
   */
  let bulkRows = $state<ProfileRow[]>([]);
  /**
   * The bulk delete's failures, by name, drawn on the row that stayed: a
   * sibling's re-read clears the panel's own reason line when it lands, so
   * a refusal's words must not live there.
   */
  let bulkFailed = new SvelteMap<string, string>();
  /** The shared browser's own row, from the last read. */
  const sharedRow = $derived(profiles.find((row) => row.name === '@shared') ?? null);
  /** The named rows: the shared browser has its own row above them. */
  const named = $derived(profiles.filter((row) => row.name !== '@shared'));
  /**
   * **This client's own takeover**, which is what its show/hide says. The
   * server's `windowed` is fleet-wide - any client may have a profile up -
   * so a row read from it offered hide on a client that was not showing it
   * (measured on the dev stack: the phone said hide while the desktop held
   * the picture).
   */
  const ownUp = $derived(surface.phase !== 'closed');
  /** Whether the SHARED browser is this client's own takeover. */
  const sharedMine = $derived(ownUp && surface.profile === '@shared');
  /** Whether any of the lines this slot reads is up as the takeover. */
  const anyUp = $derived(ownUp || profiles.some((row) => row.windowed));
  /** Whether THIS client has `name` up as the takeover. */
  function fronting(name: string): boolean {
    return ownUp && surface.profile === name;
  }
  /** Whether any profile's host is up: the driven mark, from the server's
   *  own rows (the phone has no attachment of its own to read it from). */
  const used = $derived(profiles.some((row) => row.up));
  /**
   * What the last read answered. "No profiles yet" is a claim about this
   * client's own state, and a read that never answered has no state to claim,
   * so the row waits - or says why - rather than asserting one.
   */
  let read = $state<'loading' | 'ready' | 'failed'>('loading');
  let why = $state<string | null>(null);
  /** Whether the shared browser's host is up, from the server's own row. */
  const hosting = $derived(sharedRow?.up ?? false);
  /** The engine behind the page: the server's own Chromium. The phone draws
   *  the same page's frames in its own view. */
  const engineLine = "the server's own Chromium";
  let segEl = $state<HTMLElement | null>(null);
  /** The panel, portaled to the body, so the leave guards can see it. */
  let panelEl = $state<HTMLElement | null>(null);
  /** The panel's measured caps, remeasured every frame while open. */
  let limits = $state('');

  $effect(() => {
    if (!open) return;
    // **While the panel is open it follows the layout it hangs in.** The
    // style is measured once at the open, and an ordinary reflow - the
    // composer growing and lifting the strip, another segment's figures
    // re-wrapping beside it - would otherwise leave the panel where the old
    // geometry put it, off the screen again. A frame loop while the panel is
    // open (it closes on leave) re-measures; the write lands only when the
    // string changes.
    let raf = requestAnimationFrame(function tick() {
      limits = panelStyle(segEl);
      raf = requestAnimationFrame(tick);
    });
    return () => cancelAnimationFrame(raf);
  });

  /**
   * What each read was issued as, so a read that lands after a newer one -
   * or after a failed raise - cannot write over what came later.
   */
  let readToken = 0;

  /**
   * The client's own profiles, read at mount and when the list opens. A
   * failed read keeps whatever the last one answered and says why.
   */
  async function readProfiles(): Promise<void> {
    const token = (readToken += 1);
    try {
      const rows = await listProfiles(slot);
      if (token !== readToken) return;
      profiles = rows;
      read = 'ready';
      why = null;
    } catch (error) {
      if (token !== readToken) return;
      read = 'failed';
      why = whyText(error);
    }
  }

  $effect(() => {
    void readProfiles();
  });

  /**
   * The takeover moving - the hardware Back on the phone, or a raise - says
   * nothing in THIS page acted, so every word re-reads the strip's rows; a
   * read made by a caller can land before the takeover did (measured: the
   * row kept saying "hide" over a surface already down).
   */
  $effect(() => {
    const onTakeover = (event: Event): void => {
      const what = (event as CustomEvent<string>).detail;
      if (what !== 'lowered' && what !== 'done' && what !== 'raised') return;
      void readProfiles();
    };
    window.addEventListener('forge-takeover', onTakeover);
    return () => window.removeEventListener('forge-takeover', onTakeover);
  });

  /**
   * How the panel closes when the pointer leaves: with a grace period, because
   * a gap sits between the toggle and the panel and a pointer crossing it
   * would otherwise never reach the rows.
   */
  let leaving: ReturnType<typeof setTimeout> | null = null;

  /** Open, and read the profiles fresh, which is what every opening does. */
  function hold(): void {
    if (leaving !== null) clearTimeout(leaving);
    leaving = null;
    open = true;
    outcome = null;
    bulkFailed.clear();
    void readProfiles();
  }

  /** Every door that closes the list takes an unconfirmed delete with it -
   *  the row's confirm or the bulk's, a click waiting to happen behind a
   *  closed panel - and a delete's outcome with it, so a reopening starts
   *  clean. */
  function shut(): void {
    open = false;
    armed = null;
    bulkRows = [];
    outcome = null;
    bulkFailed.clear();
  }

  function arm(): void {
    if (leaving !== null) clearTimeout(leaving);
    leaving = setTimeout(() => {
      leaving = null;
      shut();
    }, 120);
  }

  /**
   * Leaving arms the close, with the two guards every sibling row carries: a
   * pointer leave arriving over the panel's own gap is not a departure while
   * the segment is still hovered, and a leave may close while the TOGGLE
   * holds focus but not while a panel control does - closing would unmount
   * the control the reader is on.
   */
  function release(event?: FocusEvent): void {
    if (event === undefined) {
      // The panel is PORTALED to the body (`use:portal`), so moving off the
      // segment into the panel is not a departure - and a panel control
      // holding focus must not close the very thing the reader is on.
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

  /** A pointer that can hover, which a finger cannot: a tap's synthesised
   *  enter must not arm the hover path, or the click that follows toggles the
   *  panel straight back shut. */
  const hovering = (event: PointerEvent) => event.pointerType === 'mouse';

  /**
   * A compatibility press is in flight.
   *
   * **A tap focuses the toggle through its compat `mousedown`**, so focus
   * opens the panel only while a press is in flight; `mouseup` ends every
   * quiet press.
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

  /**
   * What the collapsed row says this slot holds.
   *
   * A count is a claim about a read, so before one has answered - or when the
   * last one failed and nothing was ever read - the row says the count is not
   * known rather than standing on a zero nothing measured. A failed refresh
   * keeps the last count, which something did measure.
   */
  const count = $derived(
    profiles.length > 0 || read === 'ready'
      ? `${profiles.length} profile${profiles.length === 1 ? '' : 's'}`
      : read === 'loading'
        ? '…'
        : 'count unknown',
  );

  // A pointer landing outside the segment closes the list, the same one look
  // every other popover on the page takes. **The panel counts as inside**:
  // it is PORTALED to the body, so it is not a descendant of the segment -
  // and without this check every pointerdown on one of its own controls
  // closed the panel before the click could land, which is how the delete
  // control came to work by keyboard alone.
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

  /** Escape closes from either of the list's controls, and focus goes back to
   * the toggle first - closing under the focused control would drop the focus
   * to the body, where Escape reaches nothing. */
  function esc(event: KeyboardEvent): void {
    if (event.key !== 'Escape') return;
    // Escape inside a confirm keeps the list: the reader is backing out of
    // the delete, not out of the panel, and the focus is kept - see backOut.
    if (bulkRows.length > 0) {
      backOutBulk();
      return;
    }
    if (armed !== null) {
      backOut();
      return;
    }
    toToggle();
    if (leaving !== null) clearTimeout(leaving);
    leaving = null;
    shut();
  }

  /**
   * The person's own look: the real browser window, over the same browser
   * the sessions drive. No session is answering a question here - it is the
   * door Ved asked for (2026-10-07), and closing the window disturbs nothing.
   */
  function show(profile: string | null = null): void {
    // **The seat rides the raise**: the server attaches the viewer to a slot's
    // browser, and a raise that named none would be refused with the surface
    // left dark. This row is the seat's own, so it is the one to name.
    void openSurface(profile, slot).then((reason) => {
      if (reason !== null) {
        // A read in flight was issued before this failure and must not
        // erase it when it lands.
        readToken += 1;
        read = 'failed';
        why = reason;
        return;
      }
      // The surface is up now: the row's button says hide from here.
      void readProfiles();
    });
  }

  /** The person's own lower: the surface goes, the browser keeps serving, and
   *  the row's button says show again. A refusal is drawn, not swallowed. */
  function lower(profile: string | null = null): void {
    void closeSurface(profile, slot).then((reason) => {
      if (reason !== null) {
        // **A read in flight was issued before this refusal and must not
        // erase it when it lands**, the same guard the close and the delete
        // carry.
        readToken += 1;
        read = 'failed';
        why = reason;
        return;
      }
      void readProfiles();
    });
  }

  /** The person's close: saves, frees the name, and the row falls away. */
  function close(row: ProfileRow): void {
    void closeProfile(row.name, slot).then(
      () => readProfiles(),
      (error: unknown) => {
        // A close the shell refused leaves the profile open, and the row says
        // so rather than vanishing over a name that is still held. **A read
        // already in flight is made stale**: an opening's read landing after
        // the refusal cleared the words the person is owed.
        readToken += 1;
        read = 'failed';
        why = whyText(error);
      },
    );
  }

  /**
   * Arm the confirm, and put focus on its Keep: the verb that armed it
   * unmounts, so without this a keyboard reader's next Escape lands on the
   * body, where nothing listens. One confirm at a time: the bulk's steps
   * aside for the row's.
   */
  function armDelete(row: ProfileRow): void {
    bulkRows = [];
    armed = row.name;
    void tick().then(() => {
      // The confirm lives in the panel, which is PORTALED to the body - so
      // the keep is found on the panel, not on the segment.
      panelEl?.querySelector<HTMLElement>(`[data-keep="${row.name}"]`)?.focus();
    });
  }

  /** Back out of the confirm with the focus kept: the toggle takes it first,
   *  like every dismissal, and the armed verb unmounts under it. */
  function backOut(): void {
    toToggle();
    armed = null;
  }

  /**
   * Arm the bulk confirm: the closed set is snapshotted so its names become
   * the list, the row confirm steps aside - one confirm at a time - and the
   * focus goes on its Keep for the same reason the row's does.
   */
  function armBulk(): void {
    armed = null;
    outcome = null;
    bulkRows = closed;
    void tick().then(() => {
      panelEl?.querySelector<HTMLElement>('[data-keep-bulk]')?.focus();
    });
  }

  /** Back out of the bulk confirm with the focus kept, like the row's own. */
  function backOutBulk(): void {
    toToggle();
    bulkRows = [];
  }

  /**
   * Focus goes back to the toggle BEFORE a confirm unmounts.
   *
   * The confirm holds focus as the delete starts, and a webview that fires
   * focusout when the focused element is removed (Chromium, WebView2,
   * Android WebView - macOS WKWebView does not) would otherwise read the
   * reader as having left the panel: `release` arms the close, and ~120ms
   * later `shut()` takes the outcome line - or a refusal's words - off
   * the screen before they are read. jsdom fires no blur on removal, so
   * no test can see that webview behaviour; what is pinned is this
   * function's own contract: the toggle holds focus by the time the confirm
   * clears.
   *
   * `pointed` takes the focus as a press settling rather than a reader
   * opening the panel: `focusIn`'s open would start a re-read whose
   * landing clears `why`, wiping a refusal's line this same call is about
   * to draw.
   */
  function detachConfirm(): void {
    pointed = true;
    toToggle();
  }

  /**
   * One delete, run the row's way: the shell call, the row's own mark while
   * it runs - a delete waits out a call in flight rather than refusing - the
   * re-read that takes the row away with its files, and `done` with the
   * shell's words when it refused. A failure leaves the profile standing;
   * where its reason is drawn is the caller's.
   */
  function runDelete(row: ProfileRow, done: (error: string | null) => void): void {
    bulkFailed.delete(row.name);
    deleting.add(row.name);
    void deleteProfile(row.name, slot).then(
      () => {
        deleting.delete(row.name);
        void readProfiles();
        done(null);
      },
      (error: unknown) => {
        deleting.delete(row.name);
        done(whyText(error));
      },
    );
  }

  /**
   * The person's delete, after the row's own confirmation: the browser goes,
   * the name goes, and the files go with them.
   *
   * The row says it is deleting while the shell works, and the outcome line
   * says what happened: the deleted profile by name, or the shell's own
   * words for a refusal. Either way the click draws its status; the panel
   * does not just close on it.
   */
  function del(row: ProfileRow): void {
    detachConfirm();
    armed = null;
    outcome = null;
    runDelete(row, (error) => {
      if (error === null) {
        outcome = `deleted the '${row.name}' profile and its saved logins`;
        return;
      }
      // **A read already in flight is made stale** for the same reason the
      // close's is: the opening's read would land over the refusal.
      readToken += 1;
      read = 'failed';
      why = error;
    });
  }

  /**
   * The bulk delete: the row's own machinery, once per name on the list the
   * confirm showed - one delete per profile, never a verb of its own. Each
   * row carries its own mark and its own outcome, and the summary line says
   * what went once every one of them has settled.
   */
  function delClosed(): void {
    detachConfirm();
    const rows = bulkRows;
    bulkRows = [];
    outcome = null;
    let left = rows.length;
    let gone = 0;
    const settled = (row: ProfileRow, error: string | null): void => {
      if (error === null) {
        gone += 1;
      } else {
        // **The reason goes on the row that stayed, not on the panel's own
        // line**: a sibling's re-read clears `why` when it lands, so a
        // refusal drawn there would be wiped by whichever delete finishes
        // next.
        bulkFailed.set(row.name, error);
      }
      left -= 1;
      if (left > 0) return;
      if (gone === 1) {
        outcome = 'deleted 1 closed profile and its saved logins';
      } else if (gone > 0) {
        outcome = `deleted ${gone} closed profiles and their saved logins`;
      }
    };
    for (const row of rows) {
      runDelete(row, (error) => settled(row, error));
    }
  }
</script>

<span class="bz-seg" class:open bind:this={segEl}>
  <button
    type="button"
    class="bz-tog"
    aria-expanded={open}
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
    {#if anyUp}
      <!-- A window is up on one of this slot's lines; the list says which,
           and that row's own button takes it down. -->
      <span class="dot ok" title="a browser window is open"></span>
    {:else if used}
      <!-- The system band's flat idle disc: the browser has been driven,
           which is a state and not motion. -->
      <span class="dot idle" title="the browser has been used"></span>
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
      aria-label="the browser's profiles"
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
          {hosting ? "the server's browser is up" : 'no browser is up'}
        </span>
        <!-- **The engine line** (Ved's cut, 2026-10-18): the page is the
             server's own Chromium, drawn by a native overlay on the desktop
             and painted on a canvas on the phone. Which one is not something
             the person should have to infer. -->
        <span class="tx bz-engine">
          {hosting ? engineLine : 'nothing to run a page'}
        </span>
      </div>

      <!-- **The shared profile is always there**, and a session reaches it
           only by naming `@shared` - its calls drive the seat's own profile
           otherwise. Listing only named ones read as "no profile" while a
           session was plainly using the browser. Incapable clients say that
           here rather than drawing a bare mark (#1839: absent is visible
           and explained). -->
      <div class="bz-it">
        {#if !capable}
          <Icon name="x" class="bad" />
        {:else if sharedRow?.busy}
          <span class="ring"></span>
        {:else if sharedRow?.up}
          <span class="dot ok"></span>
        {:else}
          <Icon name="x" class="bad" />
        {/if}
        <span class="nm">shared</span>
        <span class="tx">
          {capable ? "@shared · the browser's own profile" : 'this client cannot show the browser'}
        </span>
        {#if capable}
          <button
            type="button"
            class="bz-take bz-show"
            aria-label={sharedMine ? 'hide the browser surface' : 'show the browser'}
            onclick={() => (sharedMine ? lower('@shared') : show('@shared'))}
            onkeydown={esc}
          >
            {sharedMine ? 'hide' : 'show'}
          </button>
        {/if}
      </div>

      <!-- **The closed ones carry a door together**, right where the rows
           they take begin: the pile that reads as noise is exactly the
           closed profiles, a live seat's among them. -->
      {#if bulkRows.length > 0}
        <div class="bz-bulk">
          <div class="bz-it">
            <span class="tx warn">their logins and cookies go with them</span>
          </div>
          {#each bulkRows as row (row.name)}
            <div class="bz-it">
              <span class="nm">{row.name}</span>
              <span class="tx">{row.owner === '' ? 'no owner' : row.owner}</span>
            </div>
          {/each}
          <div class="bz-it">
            <button
              type="button"
              class="bz-take bz-del confirm"
              aria-label="delete the closed profiles and their logins"
              onclick={delClosed}
              onkeydown={esc}
            >
              delete them
            </button>
            <button
              type="button"
              class="bz-take"
              data-keep-bulk
              aria-label="keep the closed profiles"
              onclick={backOutBulk}
              onkeydown={esc}
            >
              keep
            </button>
          </div>
        </div>
      {:else if closed.length > 0 && deleting.size === 0}
        <div class="bz-it">
          <span class="tx">{closed.length} closed</span>
          <!-- The door waits while any delete is running: a second arming
               would list rows already on their way out. -->
          <button
            type="button"
            class="bz-take bz-del bz-del-all"
            aria-label="delete all closed profiles and their logins"
            onclick={armBulk}
            onkeydown={esc}
          >
            delete all
          </button>
        </div>
      {/if}

      {#each named as row (row.name)}
        {#if armed === row.name}
          <!-- **The confirm happens in the row itself** (Ved's pick,
               2026-10-11): the warning takes the row's own text line and the
               pair replaces the verbs - the action is the row's, so its
               answer is too. -->
          <div class="bz-it">
            <span class="ring"></span>
            <span class="nm">{row.name}</span>
            <span class="tx warn">its logins and cookies go with it</span>
            <button
              type="button"
              class="bz-take bz-del confirm"
              aria-label="delete the {row.name} profile and its logins for good"
              onclick={() => del(row)}
              onkeydown={esc}
            >
              delete it
            </button>
            <button
              type="button"
              class="bz-take"
              data-keep={row.name}
              aria-label="keep the {row.name} profile"
              onclick={backOut}
              onkeydown={esc}
            >
              keep
            </button>
          </div>
        {:else if deleting.has(row.name)}
          <div class="bz-it">
            <span class="ring"></span>
            <span class="nm">{row.name}</span>
            <span class="tx">deleting...</span>
          </div>
        {:else if row.unreadable}
          <!-- **A row this page cannot act on is drawn as itself** (rule 25),
               and carries no verbs: either the server sent it in a form this
               page cannot read, or the owner record it carries could not be
               read, so no owner can be named for the verb. -->
          <div class="bz-it">
            <Icon name="x" class="bad" />
            <span class="nm">{row.name}</span>
            <span class="tx bad">its owner record could not be read</span>
          </div>
        {:else}
          <div class="bz-it">
            {#if row.busy}
              <!-- The work spinner: a call is running on this profile now. -->
              <span class="ring"></span>
            {:else if row.up}
              <span class="dot ok"></span>
            {:else}
              <Icon name="x" class="bad" />
            {/if}
            <span class="nm">{row.name}</span>
            {#if bulkFailed.has(row.name)}
              <!-- A bulk delete the shell refused, in its own words, on the
                   row that stayed. -->
              <span class="tx bad">{bulkFailed.get(row.name) ?? ''}</span>
            {:else}
              <span class="tx">{stateText(row)}</span>
            {/if}
            <!-- **The row's own door to its own browser**, the shared row's
                 show one level down: this profile's page in the takeover. -->
            <button
              type="button"
              class="bz-take bz-show"
              aria-label={fronting(row.name)
                ? `hide the ${row.name} profile's browser`
                : `show the ${row.name} profile's browser`}
              onclick={() => (fronting(row.name) ? lower(row.name) : show(row.name))}
              onkeydown={esc}
            >
              {fronting(row.name) ? 'hide' : 'show'}
            </button>
            <!-- **A closed profile has no run to close**: it is known only
                 on disk, and close would ask the server to end a browser
                 that is not there. -->
            {#if row.owner !== '' && !row.closed}
              <button
                type="button"
                class="bz-take bz-close"
                aria-label="close the {row.name} profile"
                onclick={() => close(row)}
                onkeydown={esc}
              >
                close
              </button>
            {/if}
            <!-- **Delete is its own verb, not a stronger close.** Close
                 frees the name and keeps the logins; this removes them, so
                 it is worn apart from the pair beside it and it confirms in
                 place. -->
            <button
              type="button"
              class="bz-take bz-del"
              aria-label="delete the {row.name} profile and its logins"
              onclick={() => armDelete(row)}
              onkeydown={esc}
            >
              delete
            </button>
          </div>
        {/if}
      {/each}
      <!-- **A delete's outcome is drawn, not implied by a row going.** The
           deleted line names what went; a refusal is the shell's own words
           in the bad tone. The empty line stands aside for both, so the
           panel never says "nothing here" under the outcome that explains
           why. -->
      {#if outcome !== null}
        <div class="bz-it"><span class="tx ok">{outcome}</span></div>
      {/if}
      {#if read === 'loading'}
        <div class="bz-it"><span class="tx">reading the profiles…</span></div>
      {:else if read === 'failed'}
        <div class="bz-it"><span class="tx bad">{why}</span></div>
      {:else if named.length === 0 && outcome === null}
        <div class="bz-it">
          <span class="tx"
            >{mine === null ? 'no named profiles yet' : 'no profiles for this session'}</span
          >
        </div>
      {/if}
    </div>
  {/if}
</span>
