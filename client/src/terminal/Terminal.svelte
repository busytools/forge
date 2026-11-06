<script lang="ts">
  import { onMount } from 'svelte';
  import { Terminal as Xterm } from '@xterm/xterm';
  import { FitAddon } from '@xterm/addon-fit';
  import { Unicode11Addon } from '@xterm/addon-unicode11';
  import { SearchAddon } from '@xterm/addon-search';
  import '@xterm/xterm/css/xterm.css';

  import { terminalRecord } from './record.svelte';
  import { terminalLink } from './link';
  import {
    attachSurface,
    closeTerminal,
    controlTerminal,
    controlling,
    handOff,
    mayType,
    onOutput,
    onScreen,
    reconcileSurfaceSize,
    surface,
    terminalStateLine,
    type TerminalPhase,
  } from './surface.svelte';

  /**
   * The raised terminal: a full-window layer under a slim bar, the browser
   * takeover's sibling.
   *
   * **xterm.js draws; the server owns the screen.** The attach's `terminal_view`
   * is a VT repaint, the output frames are the shell's own bytes, and the
   * keystrokes go back as bytes. Nothing here interprets a command.
   */
  let host = $state<HTMLElement | null>(null);
  let term: Xterm | null = null;
  let phase = $state<TerminalPhase>('opening');
  let reason = $state<string | null>(null);
  /** The seat's terminal as its record carries it, which the update keeps fresh. */
  const summary = $derived(terminalRecord.terminal);
  let mounted = true;
  /** Whether this mount pushed a page-history entry. */
  let pushed = false;

  function token(name: string, fallback: string): string {
    if (typeof window === 'undefined') return fallback;
    const value = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
    return value === '' ? fallback : value;
  }

  /** The palette from the one token set; the hex values are its fallbacks for
   *  a token the theme does not set. */
  function palette() {
    return {
      background: token('--bg', '#04050a'),
      foreground: token('--text', '#c9d1d9'),
      cursor: token('--accent', '#f47600'),
      cursorAccent: token('--bg', '#04050a'),
      selectionBackground: token('--s3', '#1b2130'),
      black: token('--ansi-black', '#1b1f27'),
      red: token('--ansi-red', '#f47067'),
      green: token('--ansi-green', '#7ee787'),
      yellow: token('--ansi-yellow', '#d29922'),
      blue: token('--ansi-blue', '#6cb6ff'),
      magenta: token('--ansi-magenta', '#dcbdfb'),
      cyan: token('--ansi-cyan', '#39c5cf'),
      white: token('--ansi-white', '#b1bac4'),
      brightBlack: token('--ansi-bright-black', '#6e7681'),
      brightRed: token('--ansi-bright-red', '#ff938a'),
      brightGreen: token('--ansi-bright-green', '#aff5b4'),
      brightYellow: token('--ansi-bright-yellow', '#e3b341'),
      brightBlue: token('--ansi-bright-blue', '#79c0ff'),
      brightMagenta: token('--ansi-bright-magenta', '#f0b6ff'),
      brightCyan: token('--ansi-bright-cyan', '#56d4dd'),
      brightWhite: token('--ansi-bright-white', '#f0f6fc'),
    };
  }

  /** The state line: what the terminal is doing, in words and not only colour. */
  const stateLine = $derived(
    terminalStateLine({
      phase,
      reason,
      terminal: summary,
      viewer: surface.viewer,
      controller: surface.controller,
      controlReason: surface.control_reason,
    }),
  );

  /** The title the bar spells: the id, the shell and its cwd, once the attach names them. */
  const title = $derived.by((): string => {
    const held = summary;
    const name = surface.id === null ? 'terminal' : `terminal ${surface.id}`;
    if (held === null) return name;
    const shell = held.shell.split('/').pop() ?? 'shell';
    return `${name} · ${shell} · ${held.cwd}`;
  });

  /**
   * Whether this client may type: the controller, or the hand-off's door
   * while nobody has claimed it.
   */
  const typing = $derived(mayType());

  function send(bytes: Uint8Array): void {
    // **The person's input is the hand-off's or their own control.** Outside
    // both this surface is a viewer: the agent owns the terminal, the server
    // refuses the bytes, and sending them here would say the keyboard worked
    // when it did not.
    if (!typing) return;
    const id = surface.id;
    if (id === null) return;
    terminalLink().input(id, bytes);
  }

  function resize(cols: number, rows: number): void {
    const seat = surface.seat;
    const id = surface.id;
    if (seat === null || id === null) return;
    terminalLink().resize(seat, id, cols, rows);
  }

  onMount(() => {
    // **The keyboard goes back where it came from.** Raising the surface
    // focuses the terminal; lowering it returns focus to the door that was
    // used, so a keyboard reader is not dropped at the top of the page.
    const previous = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    const instance = new Xterm({
      allowProposedApi: true,
      cursorBlink: true,
      fontFamily: token('--mono', 'ui-monospace, Menlo, monospace'),
      fontSize: 13,
      lineHeight: 1.15,
      screenReaderMode: true,
      scrollback: 2000,
      theme: palette(),
    });
    const fitted = new FitAddon();
    instance.loadAddon(fitted);
    const unicode = new Unicode11Addon();
    instance.loadAddon(unicode);
    instance.unicode.activeVersion = '11';
    instance.loadAddon(new SearchAddon());
    // **The addons that touch the browser's globals load lazily.** Their UMD
    // wrappers read `self`, which a node test environment has no name for,
    // and a page that never mounts a terminal should not pay for them.
    void (async () => {
      try {
        const { ClipboardAddon } = await import('@xterm/addon-clipboard');
        instance.loadAddon(new ClipboardAddon());
      } catch {
        // Copy and paste stay the browser's own.
      }
      try {
        const { ImageAddon } = await import('@xterm/addon-image');
        instance.loadAddon(new ImageAddon());
      } catch {
        // Sixel and iTerm2 IIP draw nothing rather than failing the surface.
      }
      // **WebGL is an upgrade, never the default**: a renderer that silently
      // fails to paint would draw an empty terminal over a live shell.
      try {
        const { WebglAddon } = await import('@xterm/addon-webgl');
        instance.loadAddon(new WebglAddon());
      } catch {
        // The DOM renderer is already drawing.
      }
    })();
    if (host !== null) {
      instance.open(host);
      fitted.fit();
    }
    // **The attach carries the grid this surface just fitted**, so the first
    // repaint draws at once instead of being skipped for another round trip.
    if (surface.phase === 'opening') {
      void attachSurface(instance.cols, instance.rows);
    }
    instance.onData((data) => {
      // **A finger has no Ctrl or Alt**, so the bar's two sticky keys turn the
      // next character the soft keyboard produces into the control byte or an
      // escape-prefixed one.
      let out = data;
      if (ctrlArmed && data.length === 1) {
        out = String.fromCharCode(data.toUpperCase().charCodeAt(0) & 0x1f);
        ctrlArmed = false;
      }
      if (altArmed) {
        out = `\u001b${out}`;
        altArmed = false;
      }
      send(new TextEncoder().encode(out));
    });
    instance.onResize(({ cols, rows }) => resize(cols, rows));
    instance.attachCustomKeyEventHandler((event) => {
      // **Cmd+W is the window's on the desktop and the tab's in a browser**,
      // never the PTY's. On the phone's hardware keyboard, where no menu or
      // tab chord takes it, this handler is what lowers the surface. Ctrl+W
      // is the shell's.
      if (event.metaKey && event.key.toLowerCase() === 'w') {
        if (event.type === 'keydown') void closeTerminal();
        return false;
      }
      return true;
    });
    term = instance;
    const stopScreen = onScreen((view) => {
      // **The repaint is laid out at the server's grid.** A client fitted to
      // its own box must not draw a wider screen into it: tell the server
      // the size it has and take the repaint that comes back.
      if (reconcileSurfaceSize(instance.cols, instance.rows)) return;
      instance.write(view.screen);
    });
    const stopOutput = onOutput((frame) => instance.write(frame.bytes));
    phase = surface.phase === 'live' ? 'live' : 'opening';
    reason = surface.reason;
    // A surface store that failed before this mount drew already says why.
    if (surface.phase === 'failed') {
      phase = 'failed';
    }
    const observer = new ResizeObserver(() => {
      if (host === null) return;
      fitted.fit();
    });
    if (host !== null) observer.observe(host);
    instance.focus();
    // **The hardware Back lowers the surface.** A pushed entry makes a
    // browser tab's Back close the terminal rather than leaving the seat.
    if (!pushed) {
      pushed = true;
      const held: unknown = history.state;
      const base =
        typeof held === 'object' && held !== null ? (held as Record<string, unknown>) : {};
      history.pushState({ ...base, forgeTerminal: true }, '');
    }
    return () => {
      mounted = false;
      stopScreen();
      stopOutput();
      observer.disconnect();
      instance.dispose();
      term = null;
      // **An entry the surface pushed is taken back with it**, whatever closed
      // it: a strip hide or a Done unmounts without going through `lower`,
      // and a stale entry would eat the reader's next Back.
      if (pushed) {
        pushed = false;
        history.back();
      }
      previous?.focus();
    };
  });

  // A store that fails while the surface is up says so in the bar.
  $effect(() => {
    if (surface.phase === 'failed') {
      phase = 'failed';
      reason = surface.reason;
    } else if (surface.phase === 'live' && phase === 'opening') {
      phase = 'live';
    }
  });

  // **Stdin is the hand-off's, and the surface says so**: outside one the
  // keyboard does nothing rather than feeding a command the person did not
  // ask to join.
  $effect(() => {
    if (term !== null) term.options.disableStdin = !typing;
  });

  // **Taking the keyboard focuses the terminal.** The person presses take
  // control and types: the next keystroke belongs to the shell, not the bar.
  $effect(() => {
    if (controlling() && term !== null) term.focus();
  });

  // **The byte stream stops while the tab is hidden.** A page that cannot draw
  // the frames would only grow the server's backlog; a resume repaints.
  $effect(() => {
    if (typeof document === 'undefined') return;
    const move = (): void => terminalLink().pause(document.hidden);
    document.addEventListener('visibilitychange', move);
    return () => {
      document.removeEventListener('visibilitychange', move);
      terminalLink().pause(false);
    };
  });

  function lower(): void {
    void closeTerminal();
    if (pushed) {
      pushed = false;
      history.back();
    }
  }

  // The person's Back while the surface is up lowers it.
  $effect(() => {
    const back = (): void => {
      if (!pushed || !mounted) return;
      pushed = false;
      void closeTerminal();
    };
    window.addEventListener('popstate', back);
    return () => window.removeEventListener('popstate', back);
  });

  /** The touch bar's sticky modifiers: the next character carries them. */
  let ctrlArmed = $state(false);
  let altArmed = $state(false);

  /** One key the touch bar sends. */
  function key(bytes: string): void {
    send(new TextEncoder().encode(bytes));
    term?.focus();
  }

  const coarse = typeof window !== 'undefined' && window.matchMedia('(pointer: coarse)').matches;
  let touch = $state(coarse);

  $effect(() => {
    if (typeof window === 'undefined') return;
    const query = window.matchMedia('(pointer: coarse)');
    const move = (): void => {
      touch = query.matches;
    };
    query.addEventListener('change', move);
    return () => query.removeEventListener('change', move);
  });
</script>

<div class="tn" role="dialog" aria-modal="true" aria-label="the seat's terminal">
  <!-- **The browser takeover's bar, not a terminal-shaped one.** The same
       `tb-` dress and the same doors: back to chat, the title, the state in
       words, take or release control, and the parked ask's Done and Not now.
       A person who has used the browser surface has used this one. -->
  <header class="tn-bar tb-row">
    <button type="button" class="tb-btn d" onclick={lower} aria-label="back to chat">
      back to chat
    </button>
    <span class="tb-title" {title}>{title}</span>
    <span
      class="tn-state"
      role="status"
      class:bad={phase === 'failed'}
      class:held={summary?.held ?? false}
      class:mine={controlling()}
    >
      {stateLine}
    </span>
    <span class="tb-acts">
      {#if phase === 'live'}
        <!-- **The person takes the keyboard explicitly**, during a hand-off
             too: the ask raises the surface as a viewer, and this press is
             what lets them type. The agent's own call takes it back. **The
             press is drawn** while the server answers, and a refusal lands
             in the state line. -->
        <button
          type="button"
          class="tb-btn"
          aria-pressed={controlling()}
          disabled={surface.control_pending !== null}
          onclick={() => controlTerminal(!controlling())}
        >
          {surface.control_pending === true
            ? 'taking control...'
            : surface.control_pending === false
              ? 'releasing...'
              : controlling()
                ? 'release control'
                : 'take control'}
        </button>
      {/if}
      {#if handOff.current !== null}
        <!-- **The parked ask's answer lives here while the surface is up**:
             the dock is behind this layer, and a hand-off with no reachable
             Done would hold the session for ever. -->
        <button type="button" class="tb-btn" onclick={() => handOff.current?.respond('done')}>
          Done
        </button>
        <button type="button" class="tb-btn d" onclick={() => handOff.current?.respond('not_now')}>
          Not now
        </button>
      {/if}
    </span>
  </header>
  {#if handOff.current !== null}
    <!-- **The ask is on the raised bar too**: the dock is behind this
         surface, and the person needs the reason for the typing they are
         about to do, and whether it is hidden. -->
    <div class="tb-ask">
      {handOff.current.reason}{handOff.current.secret ? ' · typing is hidden' : ''}
    </div>
  {/if}

  {#if phase === 'failed'}
    <div class="tn-state-page" role="status">
      <p>{reason ?? 'the terminal stopped answering'}</p>
    </div>
  {:else}
    <div class="tn-body">
      <div class="tn-screen" bind:this={host}></div>
      {#if phase === 'opening'}
        <p class="tn-opening" role="status">opening the terminal…</p>
      {/if}
    </div>
  {/if}

  {#if touch && phase === 'live'}
    <div class="tn-keys" role="toolbar" aria-label="terminal keys" aria-disabled={!typing}>
      <button
        type="button"
        aria-pressed={ctrlArmed}
        disabled={!typing}
        onclick={() => {
          ctrlArmed = !ctrlArmed;
        }}
      >
        Ctrl
      </button>
      <button
        type="button"
        aria-pressed={altArmed}
        disabled={!typing}
        onclick={() => {
          altArmed = !altArmed;
        }}
      >
        Alt
      </button>
      <button type="button" disabled={!typing} onclick={() => key('\x1b')}> Esc </button>
      <button type="button" disabled={!typing} onclick={() => key('\t')}> Tab </button>
      <button type="button" disabled={!typing} onclick={() => key('\x03')}> ^C </button>
      <button type="button" disabled={!typing} onclick={() => key('\x1b[A')}> ↑ </button>
      <button type="button" disabled={!typing} onclick={() => key('\x1b[B')}> ↓ </button>
      <button type="button" disabled={!typing} onclick={() => key('\x1b[D')}> ← </button>
      <button type="button" disabled={!typing} onclick={() => key('\x1b[C')}> → </button>
      <button type="button" disabled={!typing} onclick={() => key('\r')}> Enter </button>
      <button type="button" disabled={!typing} onclick={() => key('/')}>/</button>
      <button type="button" disabled={!typing} onclick={() => key('-')}>-</button>
      <button type="button" disabled={!typing} onclick={() => key('|')}>|</button>
      <button type="button" disabled={!typing} onclick={() => key('~')}>~</button>
    </div>
  {/if}
</div>

<style>
  .tn {
    position: fixed;
    inset: 0;
    z-index: 40;
    display: flex;
    flex-direction: column;
    background: var(--bg);
  }
  /* The bar's own dress is the browser takeover's (`tb-row`/`tb-btn` in
     web.css); this carries only what is the terminal's - the full-window
     layer's background and the state line in words. */
  .tn-bar {
    border-bottom: 1px solid var(--line);
    background: var(--s1);
    color: var(--text);
  }
  .tn-state {
    color: var(--muted);
    font-size: 12.5px;
    white-space: nowrap;
  }
  .tn-state.held {
    color: var(--warn);
  }
  .tn-state.mine {
    color: var(--accent);
  }
  .tn-state.bad {
    color: var(--bad);
  }
  .tn-body {
    position: relative;
    flex: 1;
    min-height: 0;
  }
  .tn-screen {
    position: absolute;
    inset: 0;
    padding: 6px;
  }
  .tn-opening {
    position: absolute;
    inset: 0;
    display: grid;
    place-items: center;
    color: var(--muted);
    pointer-events: none;
  }
  .tn-state-page {
    flex: 1;
    display: grid;
    place-items: center;
    color: var(--bad);
    padding: 24px;
  }
  .tn-keys {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
    padding: 8px;
    border-top: 1px solid var(--line);
    background: var(--s1);
  }
  .tn-keys button {
    min-width: 44px;
    min-height: 44px;
    border: 1px solid var(--line);
    border-radius: 8px;
    background: var(--s2);
    color: var(--text);
    font: inherit;
  }
  /* **An inert key is drawn inert**: the key bar keeps its own colours, so
     the state has to be said rather than left to the platform's default. */
  .tn-keys button:disabled {
    opacity: 0.45;
    cursor: default;
  }
  .tn-keys button:disabled:hover {
    background: var(--s2);
  }
</style>
