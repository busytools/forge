<script lang="ts">
  import Icon from '../components/Icon.svelte';
  import type { Connection } from '../socket';
  import type { SessionSlot } from '../wire/types';
  import type { Ask, PermissionOption } from './wire';

  let {
    ask,
    slot,
    connection,
    depth = 1,
    notice = null,
    onanswer = () => {},
  }: {
    ask: Ask;
    slot: SessionSlot;
    connection: Pick<Connection, 'dispatch'>;
    /** How many prompts wait behind this one, which the queue line states. */
    depth?: number;
    /** Why the core refused an answer to this prompt, when it did. */
    notice?: string | null;
    onanswer?: (toolId: string | null) => void;
  } = $props();

  /** Which sprite carries an option's meaning, and the colour the sheet gives it. */
  const ICONS: Record<PermissionOption['kind'], string> = {
    allow: 'check',
    deny: 'x',
    edit: 'edit',
    notes: 'dots',
  };
  const TONES: Record<PermissionOption['kind'], string> = {
    allow: 'ok',
    deny: 'no',
    edit: 'ed',
    notes: '',
  };

  /**
   * One row as the dock draws it, whichever prompt it came from.
   *
   * A question's rows and a permission's are the same row with a different
   * marker - a checkbox where a question draws one, the option's own icon
   * where a permission does - so they are built once here rather than drawn
   * twice below.
   */
  interface Row {
    key: string;
    /** What the row's own marker is: the meaning's icon, or the question's box. */
    icon: string | null;
    tone: string;
    label: string;
    /** The option this row answers with, or `null` for the question's own words. */
    optionId: string | null;
  }

  const rows = $derived<Row[]>(rowsOf(ask));
  const question = $derived(ask.kind === 'question');

  function rowsOf(prompt: Ask): Row[] {
    if (prompt.kind === 'permission') {
      return prompt.request.options.map((option) => ({
        key: option.optionId,
        icon: ICONS[option.kind],
        tone: TONES[option.kind],
        label: option.name,
        optionId: option.optionId,
      }));
    }
    if (prompt.kind === 'question') {
      return [
        ...prompt.request.options.map((option) => ({
          key: option.optionId,
          icon: null,
          tone: 'ok',
          label: option.label,
          optionId: option.optionId,
        })),
        // The escape hatch the mockup draws: words rather than a choice, which
        // is an answer the core accepts with nothing selected.
        {
          key: 'own',
          icon: null,
          tone: 'ok',
          label: 'Tell Claude something else:',
          optionId: null,
        },
      ];
    }
    return [];
  }

  /** Which row a key would take, which is the first until one moves it. */
  let marked = $state(0);

  /**
   * Answer with the option the core offered.
   *
   * The outcome is built from the option the core sent rather than from
   * anything this component invented: an option's own `action` is what decides
   * what happens, and a client naming its own could allow what the prompt never
   * offered.
   */
  function answer(at: number): void {
    const row = rows[at];
    if (row === undefined) return;
    const toolId = ask.kind === 'permission' || ask.kind === 'question' ? ask.request.toolId : '';
    onanswer(toolId === '' ? null : toolId);
    if (ask.kind === 'permission') {
      // The outcome rides the subscription rather than a reply, so there is
      // nothing here to await: the dock closes when the core's record says the
      // prompt is gone.
      void connection.dispatch({
        respond_permission: {
          key: slot,
          tool_id: toolId,
          outcome: {
            outcome: 'selected',
            option_id: row.optionId,
            action: optionAction(row.optionId),
          },
        },
      });
      return;
    }
    if (ask.kind === 'question') {
      void connection.dispatch({
        respond_question: {
          key: slot,
          tool_id: toolId,
          outcome: {
            outcome: 'answered',
            selected_option_ids: row.optionId === null ? [] : [row.optionId],
            annotation: null,
          },
        },
      });
    }
  }

  /** The dispatch routing the core set for an option, echoed back untouched. */
  function optionAction(optionId: string | null): Record<string, unknown> {
    if (ask.kind !== 'permission') return {};
    return (
      ask.request.options.find((option) => option.optionId === optionId)?.action ?? {
        kind: 'deny',
      }
    );
  }

  function move(step: number): void {
    if (rows.length === 0) return;
    marked = (marked + step + rows.length) % rows.length;
  }

  /**
   * A row's own key, which is the listbox's rule applied to the row a focus
   * landed on: the arrows move the mark from the list, and a row answers.
   */
  function onRowKey(event: KeyboardEvent, at: number): void {
    if (event.key !== 'Enter' && event.key !== ' ') return;
    event.preventDefault();
    answer(at);
  }

  /** The id the listbox points at, which is how a reader hears which row is marked. */
  function rowId(at: number): string {
    const row = rows[at];
    const toolId = ask.kind === 'permission' || ask.kind === 'question' ? ask.request.toolId : '';
    return `dock-${toolId}-${row === undefined ? at : row.key}`;
  }

  /** The keys a dock answers to, which are the ones that can do what they say. */
  function onkey(event: KeyboardEvent): void {
    if (event.key === 'ArrowDown') {
      event.preventDefault();
      move(1);
      return;
    }
    if (event.key === 'ArrowUp') {
      event.preventDefault();
      move(-1);
      return;
    }
    if (event.key === 'Enter' && rows.length > 0) {
      event.preventDefault();
      answer(marked);
      return;
    }
    // A question's rows draw a box per option and its answer carries one of
    // them, so a reject key would name a key that cannot do what it says.
    if (event.key === 'Escape' && !question && rows.length > 0) {
      event.preventDefault();
      const deny = rows.findIndex((row) => row.tone === 'no');
      answer(deny < 0 ? rows.length - 1 : deny);
    }
  }
</script>

<div class="dock">
  {#if depth > 1}
    <div class="queue">▼ {depth - 1} more pending after this</div>
  {/if}
  {#if notice !== null}
    <div class="notice bad">{notice}</div>
  {/if}

  {#if ask.kind === 'permission'}
    <div class="head">
      <span class="t mono">{ask.request.title}</span>
      {#if ask.request.subject !== ''}
        <span class="q">{ask.request.subject}</span>
      {/if}
    </div>
    {#if ask.request.reason !== null}
      <div class="reason">{ask.request.reason}</div>
    {/if}
    {#if ask.request.description !== null}
      <div class="desc">{ask.request.description}</div>
    {/if}
  {:else if ask.kind === 'question'}
    <div class="head">
      <span class="qm">?</span>
      <span class="t">{ask.request.header}</span>
      <span class="q">Q{ask.request.index + 1} of {ask.request.total}</span>
    </div>
    <div class="desc">{ask.request.question}</div>
  {:else}
    <div class="head"><span class="t">a held post is waiting for you</span></div>
    <div class="desc">its options arrived before this view attached</div>
  {/if}

  {#if rows.length > 0}
    <div
      class="opts"
      role="listbox"
      aria-label="the prompt's options"
      tabindex="0"
      aria-activedescendant={rowId(marked)}
      onkeydown={onkey}
    >
      {#each rows as row, at (row.key)}
        <div
          class="opt"
          class:sel={at === marked}
          role="option"
          id={rowId(at)}
          aria-selected={at === marked}
          tabindex="-1"
          onclick={() => answer(at)}
          onkeydown={(event) => onRowKey(event, at)}
        >
          {#if row.icon !== null}
            <Icon name={row.icon} class={row.tone} />
          {:else}
            <span class="box2"></span>
          {/if}
          <span class="lbl">{row.label}</span>
        </div>
      {/each}
    </div>
    <div class="keys">
      <span><kbd>↑</kbd><kbd>↓</kbd> {question ? 'move' : 'select'}</span>
      <span><kbd>Enter</kbd> {question ? 'submit' : 'confirm'}</span>
      {#if !question}
        <span><kbd>Esc</kbd> reject</span>
      {/if}
    </div>
  {/if}
</div>
