<script lang="ts">
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
   * is a no-op. The row is a tool row like any other - the lane says the kind,
   * the mark says whether the run exited clean, and the name and the hook's own
   * words read as the call's own title does - because a hook IS work the
   * session ran, and anything else makes it a second system inside the group.
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
  let { run }: { run: HookRun } = $props();

  /** The closed row's tail: the hook's own words in one line, or null where it said nothing. */
  const tail = $derived(hookTail(run.body));

  function hookTail(body: string | null): string | null {
    if (body === null) return null;
    const said = hookWords(body);
    return said.trim() === '' ? null : renderInlineProse(joinedLine(said));
  }

  /**
   * What a hook's output says.
   *
   * **The two places the CLI's own envelopes carry text, and only those**: the
   * context a session-start hook injects (`hookSpecificOutput.additionalContext`)
   * and the line a hook shows the reader (`systemMessage`, top level). A key in
   * the other half is read by nobody - the CLI would not have shown it either -
   * so it draws no tail rather than words the session never saw.
   */
  function hookWords(body: string): string {
    const text = body.trim();
    if (!text.startsWith('{')) return body;
    let held: unknown;
    try {
      held = JSON.parse(text);
    } catch {
      return body;
    }
    if (held === null || typeof held !== 'object' || Array.isArray(held)) return body;
    const fields = held as Record<string, unknown>;
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

<details class="leaf hookrow">
  <summary>
    {#if run.failed}
      <Icon name="x" class="st err" />
    {:else}
      <Icon name="check" class="st" />
    {/if}
    <span class="tn"
      >{run.name}{#if run.event !== null}
        ({run.event}){/if}</span
    >
    {#if tail !== null}
      <!--
        The row's rendered line, which the module produced from escaped input:
        the same renderer the body and a thought's line use, raw HTML off.
      -->
      <!-- eslint-disable-next-line svelte/no-at-html-tags -->
      <span class="ev">{@html tail}</span>
    {/if}
    <Chevron />
  </summary>
  {#if run.body !== null}
    <div class="body">
      <div class="term">{run.body}</div>
    </div>
  {/if}
</details>
