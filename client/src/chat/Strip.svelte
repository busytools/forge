<script lang="ts">
  import Icon from '../components/Icon.svelte';
  import { money, tokens } from './numbers';
  import { attributed, cached, elapsed } from './report';
  import type { TurnInfo } from './units';

  /**
   * A turn's metadata row: the mark, its clock, and the figures the CLI
   * reported.
   *
   * **One row drawn in two places** - under a settled turn, and pinned above
   * the box while one runs - so it renders its fields with no wrapper of its
   * own: each caller's element is the flex row, and a wrapper would put a
   * second box between the fields and the thing that wraps them.
   *
   * **A field the record does not carry is dropped, never drawn as a zero or a
   * dash.** A running row grows as its frames land, and neither a zero nor a
   * placeholder is a figure the CLI reported.
   */
  let { info }: { info: TurnInfo } = $props();

  /** The record with an unattributed usage block dropped, which is the rule `report.ts` holds. */
  const held = $derived(attributed(info));

  /**
   * The tick a running row's clock moves on.
   *
   * The fold's span only grows when a frame lands, and a live turn can wait
   * minutes on one call, so the row would otherwise freeze mid-turn - which is
   * the very stretch a reader is watching the clock through.
   */
  let now = $state(Date.now());
  /**
   * The running fact as a value, so the timer does not restart per frame.
   *
   * **Reading `held.running` in the effect below restarts the timer on every
   * frame**: `held` is a fresh object each time a frame lands, so the tracked
   * dependency changes, the cleanup clears the interval before it can fire,
   * and the clock freezes while frames stream - then jumps when they pause
   * (Ved, 2026-10-09: "it stops and then it just jumps to a bigger value").
   * A boolean changes only when the state flips.
   */
  const running = $derived(held.running);
  $effect(() => {
    if (!running) return;
    const id = setInterval(() => {
      now = Date.now();
    }, 1000);
    return () => clearInterval(id);
  });

  const clock = $derived(elapsed(held, now));
  const share = $derived(cached(held));
</script>

<!-- The mark follows the turn: a turn that did not finish leads with the
     failure mark, and the line under this row carries its words. A check above
     "Turn failed" is two signals disagreeing on one row. -->
{#if held.running}
  <span class="st"><span class="ring"></span></span>
{:else if held.failed}
  <Icon name="x" class="st err" />
{:else}
  <Icon name="check" class="st" />
{/if}
<span>{clock}</span>
{#if held.running && held.thinking_tokens !== null}
  <span class="sep">{'\u{b7}'}</span>
  <span>thinking {tokens(held.thinking_tokens)}</span>
{/if}
{#if held.input_tokens !== null}
  <span class="sep">{'\u{b7}'}</span>
  <span
    >{tokens(held.input_tokens)}{'\u{2191}'}{#if held.output_tokens !== null}
      {tokens(held.output_tokens)}{'\u{2193}'}{/if}</span
  >
{/if}
{#if share !== null}
  <span class="sep">{'\u{b7}'}</span>
  <span>{share}% cached</span>
{/if}
{#if held.cache_written_tokens !== null}
  <span class="sep">{'\u{b7}'}</span>
  <span>{tokens(held.cache_written_tokens)} written</span>
{/if}
<!-- The cumulative cost is the one figure only the Result carries, so a
     running row has none: the segment is dropped rather than drawn with a
     placeholder in it, which would read as a figure rather than as one it has
     not been given. -->
{#if held.session_cost_usd !== null}
  <span class="sep">{'\u{b7}'}</span>
  <span>{money(held.session_cost_usd)} cumulative</span>
{/if}
