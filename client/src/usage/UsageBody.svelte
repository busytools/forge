<script lang="ts">
  /**
   * The usage the page draws: the lifetime headline and its caption, the
   * three rolling totals, the split, and the table, for a chosen window and
   * grouping.
   *
   * The terminal's `/usage` overlay is the reference for every word and
   * figure here (`crates/forge-tui/src/ui/usage_overlay.rs`, and the book's
   * `ui/usage.md`): the unit scales, the dash where no price stood, the
   * caption's two states, the shares. What is this side's own is the
   * drawing: a grid instead of a fixed-width table, controls that are
   * buttons rather than keys.
   */

  import Brand from '../components/Brand.svelte';
  import type { UsageReport } from '../wire/usage';
  import {
    WINDOWS,
    headline,
    quickTotals,
    split,
    tableRows,
    type Grouping,
    type WindowKey,
  } from './view';

  let {
    wire,
    mark = null,
    onrefresh,
  }: { wire: UsageReport; mark?: string | null; onrefresh: () => void } = $props();

  // The terminal's own defaults: grouped by project, over the lifetime.
  let period = $state<WindowKey>('lifetime');
  let group = $state<Grouping>('project');

  const head = $derived(headline(wire));
  const totals = $derived(quickTotals(wire));
  const parts = $derived(split(wire));
  const table = $derived(tableRows(wire, period, group));
</script>

<main class="wrap usage">
  <header class="top pagetop">
    <div class="brand">
      <Brand name={mark} />
      <span class="word">forge</span>
    </div>
    <div class="title">
      <h1 class="t">Usage</h1>
      <span class="s">tokens &middot; cost &middot; by project or model</span>
    </div>
    <a class="back" href="/">&larr; home</a>
  </header>

  <section class="sum" aria-label="lifetime">
    <div class="head">
      <span class="lbl">lifetime</span>
      <span class="big">{head.tokens}</span>
      <span class="dk">tokens</span>
      <!-- The ≈ marks the cost as notional, so the figure and the caption
           below say the same thing. -->
      {#if head.cost !== null}
        <span class="cost">{head.cost}&asymp;</span>
      {:else}
        <span class="cost off">-</span>
      {/if}
      <span class="counts">{head.projects} projects &middot; {head.models} models</span>
      <!-- `dim` is the class the sheet already paints, so only the warning
           tone is worn; the base caption rule is dim. -->
      <span class="cap {head.caption.tone === 'warn' ? 'warn' : ''}">{head.caption.text}</span>
    </div>

    <div class="roll">
      {#each totals as total (total.label)}
        <span class="tot">
          <span class="k">{total.label}</span>
          <span class="n">{total.tokens}</span>
          <span class="c">{total.cost}</span>
        </span>
      {/each}
    </div>

    <div class="split">
      <span class="k">split</span>
      <span class="in">input <b>{parts.input}</b></span>
      <span class="cw">cache-write <b>{parts.cacheWrite}</b></span>
      <span class="cr">
        cache-read <b>{parts.cacheRead}</b>{#if parts.cacheReadShare !== null}<b
            >({parts.cacheReadShare})</b
          >{/if}
      </span>
      <span class="out">output <b>{parts.output}</b></span>
    </div>
  </section>

  <div class="controls">
    <div class="sel" role="group" aria-label="grouping">
      <span class="k">group</span>
      <button class="pick" aria-pressed={group === 'project'} onclick={() => (group = 'project')}
        >by project</button
      >
      <button class="pick" aria-pressed={group === 'model'} onclick={() => (group = 'model')}
        >by model</button
      >
    </div>
    <div class="sel" role="group" aria-label="window">
      <span class="k">window</span>
      {#each WINDOWS as option (option.key)}
        <button
          class="pick"
          aria-pressed={period === option.key}
          onclick={() => (period = option.key)}>{option.label}</button
        >
      {/each}
    </div>
    <!-- The subject scans once per ask and then hears nothing, so fresh
         figures are this press rather than a push. -->
    <button class="re" onclick={onrefresh}>refresh</button>
  </div>

  <table class="tbl" aria-label="token usage by {group === 'model' ? 'model' : 'project'}">
    <thead>
      <tr>
        <th scope="col">{group === 'model' ? 'model' : 'project'}</th>
        <th scope="col">input</th>
        <th scope="col">cache&middot;wr</th>
        <th scope="col">cache&middot;rd</th>
        <th scope="col">output</th>
        <th scope="col">tokens</th>
        <th scope="col">cost&asymp;</th>
      </tr>
    </thead>
    <tbody>
      {#each table.body as row (row.label)}
        <tr class:quiet={row.quiet}>
          <th scope="row">{row.label}</th>
          <td class="in">{row.input}</td>
          <td class="cw">{row.cacheWrite}</td>
          <td class="cr">{row.cacheRead}</td>
          <td class="out">{row.output}</td>
          <td class="tk">{row.tokens}</td>
          <td class="cost {row.cost === '-' ? 'off' : row.approx ? 'gpt' : ''}">
            {row.cost}
            {#if row.approx}<span class="approx">(GPT approx)</span>{/if}
          </td>
        </tr>
      {/each}
    </tbody>
    <tfoot>
      <tr class="total">
        <th scope="row">{table.total.label}</th>
        <td class="in">{table.total.input}</td>
        <td class="cw">{table.total.cacheWrite}</td>
        <td class="cr">{table.total.cacheRead}</td>
        <td class="out">{table.total.output}</td>
        <td class="tk">{table.total.tokens}</td>
        <td class="cost">{table.total.cost}</td>
      </tr>
    </tfoot>
  </table>
</main>
