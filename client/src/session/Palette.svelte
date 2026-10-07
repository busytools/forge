<script lang="ts">
  import { untrack } from 'svelte';

  import Icon from '../components/Icon.svelte';
  import { goTo, parseRoute } from '../routes';
  import { report, type Connection } from '../socket';
  import type { HomeWire } from '../wire/home';
  import { mintPromptId } from '../wire/ids';
  import type { SessionSlot } from '../wire/types';
  import { closeSeat } from './close';
  import { paletteRows, type PaletteRow } from './view';

  /**
   * The command palette: Cmd+K's own surface, and the touch door to the same
   * things a keyboard reaches - the fleet, forge's commands and the doings.
   *
   * **One flat cursor over grouped rows.** The sections are headings; the
   * walk is by row, and it starts on the current project's lead so Cmd+K
   * then Enter lands on it. Type to filter across every group, Enter opens
   * or runs, and Cmd+K again or Escape closes.
   */
  let {
    open,
    wire,
    slot,
    connection,
    sessionId,
    onclose,
    onpeek,
  }: {
    open: boolean;
    wire: HomeWire;
    slot: SessionSlot;
    connection: Connection;
    /** The shown seat's occupant id, which the copy doing carries. */
    sessionId: string | null;
    onclose: () => void;
    /** Open the projects rail, over this page. */
    onpeek: () => void;
  } = $props();

  let needle = $state('');
  let cursor = $state(0);
  let field = $state<HTMLInputElement | null>(null);

  const shown = $derived.by(() => {
    const q = needle.trim().toLowerCase();
    const match = (row: PaletteRow) =>
      q === '' || `${row.label} ${row.detail}`.toLowerCase().includes(q);
    return paletteRows(wire, slot)
      .map((section) => ({ title: section.title, rows: section.rows.filter(match) }))
      .filter((section) => section.rows.length > 0);
  });
  const flat = $derived(shown.flatMap((section) => section.rows));

  // Opening resets the walk and starts it on the lead. The body is untracked
  // whole: it reads `flat` and writes state, and a registered read would make
  // every arriving home frame reset the cursor under the reader.
  $effect(() => {
    if (!open) return;
    untrack(() => {
      needle = '';
      const at = flat.findIndex((row) => row.lead === true);
      cursor = at === -1 ? 0 : at;
      field?.focus();
    });
  });

  function onkey(event: KeyboardEvent): void {
    if (event.key === 'ArrowDown') {
      event.preventDefault();
      cursor = Math.min(cursor + 1, flat.length - 1);
      return;
    }
    if (event.key === 'ArrowUp') {
      event.preventDefault();
      cursor = Math.max(cursor - 1, 0);
      return;
    }
    if (event.key === 'Enter') {
      event.preventDefault();
      const row = flat[cursor];
      if (row !== undefined) run(row);
      return;
    }
    if (event.key === 'Escape') {
      event.preventDefault();
      onclose();
    }
  }

  /** What a row does when it is opened: go, send, or a doing. */
  function run(row: PaletteRow): void {
    if (row.href !== undefined) {
      goTo(parseRoute(row.href));
      onclose();
      return;
    }
    if (row.text !== undefined) {
      // Exactly what the composer sends for a typed command, id included:
      // the CLI emits that prompt's lifecycle frames only under our uuid.
      void connection.dispatch({
        prompt_under: {
          key: slot,
          text: row.text,
          attachments: [],
          uuid: mintPromptId(),
          source: 'you',
        },
      });
      onclose();
      return;
    }
    if (row.doing === 'peek') {
      onclose();
      onpeek();
      return;
    }
    if (row.doing === 'copy') {
      const id = sessionId;
      if (id === null || typeof navigator === 'undefined') return;
      if (navigator.clipboard === undefined) {
        report('no clipboard on this page', row.label);
        onclose();
        return;
      }
      navigator.clipboard.writeText(id).then(
        () => onclose(),
        (why: unknown) => report('the clipboard refused the session id', why),
      );
      return;
    }
    if (row.doing === 'close') {
      closeSeat(connection, wire, slot, slot, Date.now());
      onclose();
    }
  }
</script>

{#if open}
  <div class="pal" role="dialog" aria-modal="true" aria-label="seats and commands">
    <!-- The scrim is a click-catcher rather than a control: Escape and the
         input's own keys are the keyboard doors, so presentation is the
         honest role. -->
    <div class="pscrim" role="presentation" onclick={onclose}></div>
    <div class="panel">
      <div class="pin">
        <Icon name="search" class="dim" />
        <input
          bind:this={field}
          bind:value={needle}
          onkeydown={onkey}
          type="text"
          data-editor="palette"
          placeholder="go to a seat, run a command, or do a thing"
          role="combobox"
          aria-expanded="true"
          aria-controls="pal-rows"
          aria-activedescendant={cursor >= 0 ? `pal-row-${flat[cursor]?.id ?? ''}` : undefined}
          autocomplete="off"
          spellcheck="false"
          aria-label="seats and commands"
        />
        <span class="esc">Esc</span>
      </div>
      <div class="rows" id="pal-rows" role="listbox" aria-label="seats and commands">
        {#if flat.length === 0}
          <div class="none">nothing matches</div>
        {:else}
          {#each shown as section (section.title)}
            <div class="grp">{section.title}</div>
            {#each section.rows as row (row.id)}
              {@const at = flat.indexOf(row)}
              <button
                type="button"
                class="it"
                class:sel={at === cursor}
                id={`pal-row-${row.id}`}
                role="option"
                tabindex={-1}
                aria-selected={at === cursor}
                onclick={() => run(row)}
                onmousemove={() => (cursor = at)}
              >
                {#if row.kind === 'seat'}
                  <span class="dot {row.mark ?? 'idle'}"></span>
                {:else}
                  <span class="gl">{row.kind === 'command' ? '/' : '\u{203a}'}</span>
                {/if}
                <span class="nm"
                  >{row.label}{#if row.lead}<span class="kbd">current</span>{/if}</span
                >
                <span class="what">{row.detail}</span>
              </button>
            {/each}
          {/each}
        {/if}
      </div>
      <div class="pfoot">
        <span>{'\u{2191}\u{2193}'} move</span><span>{'\u{21a9}'} open</span>
        <span>Cmd+K or Esc closes</span>
      </div>
    </div>
  </div>
{/if}
