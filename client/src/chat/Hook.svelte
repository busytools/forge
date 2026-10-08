<script lang="ts">
  import { untrack } from 'svelte';

  import Chevron from '../components/Chevron.svelte';
  import Icon from '../components/Icon.svelte';
  import { renderInlineProse } from './prose';
  import { joinedLine } from './text';
  import type { HookRun } from './units';

  /**
   * One hook's own run: the start that named it, what it printed as it went,
   * and the response that settled it.
   *
   * **The terminal draws nothing for this**, which is why the shape is the
   * client's to choose rather than a parity port: its arm for the three frames
   * is a no-op. The row is a tool row like any other - the glyph says the kind
   * and its colour says whether the run exited clean, and the name and the
   * hook's own words read as the call's own title does - because a hook IS
   * work the session ran, and anything else makes it a second system inside
   * the group.
   *
   * **What the hook printed is not what it said.** A session-start hook prints
   * the markdown it injects - whose headings and lists are marks a one-line row
   * cannot draw, so the line read `## The PR review gate` - or a JSON envelope
   * the CLI reads, whose braces are punctuation and whose text sits inside it,
   * so the line read `{` (Ved, 2026-10-03).
   *
   * The body still carries the output VERBATIM behind the row's own open - a
   * hook's output is long and secondary, and a summary standing in for it would
   * be the drop rule 25 forbids - while the tail is the words joined, drawn as
   * the thought row's own line is: inline marks rendered, the layout's ellipsis
   * where they run out.
   */
  let { run, open = false }: { run: HookRun; open?: boolean } = $props();

  /** Whether the row is open; a closed row carries its summary and nothing else. */
  let opened = $state(untrack(() => open));

  /** The closed row's tail: the hook's own words in one line, or null where it said nothing. */
  const tail = $derived(hookTail(run.body));

  function hookTail(body: string | null): string | null {
    if (body === null) return null;
    // **The check is on the joined words, not on the raw output.** Output that
    // is nothing but block marks - a fence, a rule - strips to '', and a check
    // made before the marks come off would draw an empty tail slot for it.
    const said = joinedLine(hookWords(body));
    return said === '' ? null : renderInlineProse(said);
  }

  /**
   * What a hook's output says.
   *
   * **The two envelope fields whose text reaches a session, and only those**:
   * the context a session-start hook injects (`hookSpecificOutput.additionalContext`)
   * and the line a hook shows the reader (`systemMessage`, top level). A key in
   * the other half draws no tail rather than guessing at a shape this row does
   * not know - **and that is a gap, not a claim that such text is not
   * shown**: the CLI does display text from `stopReason`, the permission
   * reasons and a top-level `reason`, and no transcript carries one today
   * (measured: 0 of 13,656 hook rows), so a reader losing that line is work
   * this row has not done yet.
   */
  function hookWords(body: string): string {
    const text = body.trim();
    if (!text.startsWith('{')) return body;
    let fields: Record<string, unknown>;
    try {
      // A text that opens `{` and parses is an object, always: no null, array
      // or primitive can open with one.
      fields = JSON.parse(text) as Record<string, unknown>;
    } catch {
      return body;
    }
    const inner = fields['hookSpecificOutput'];
    const context =
      inner !== null && typeof inner === 'object' && !Array.isArray(inner)
        ? (inner as Record<string, unknown>)['additionalContext']
        : null;
    for (const said of [context, fields['systemMessage']]) {
      if (typeof said === 'string' && said.trim() !== '') return said;
    }
    return '';
  }
</script>

<details class="leaf hookrow" bind:open={opened}>
  <summary>
    <Icon name="hook" class={`gl${run.failed ? ' err' : ' ok'}`} />
    <span class="tn"
      >{run.name}{#if run.event !== null}
        ({run.event}){/if}</span
    >
    {#if tail !== null}
      <!-- eslint-disable-next-line svelte/no-at-html-tags -->
      <span class="ev">{@html tail}</span>
    {/if}
    <Chevron />
  </summary>
  {#if opened && run.body !== null}
    <div class="body">
      <div class="term">{run.body}</div>
    </div>
  {/if}
</details>
