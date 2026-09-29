<script lang="ts">
  import type { AccountView } from './view';

  /**
   * The account chip: the account this project would spawn under, opening on
   * what the poller knows about it.
   *
   * It is a disclosure rather than a popover the page positions, because the
   * thing it opens on is an answer to a question the reader asked by clicking,
   * and the browser already knows how to close one.
   */
  let { view }: { view: AccountView } = $props();
</script>

<details class="acct" data-k="acct">
  <summary><span class="dot idle"></span>{view.name}</summary>
  <div class="pop">
    <div class="hd">
      <span class="nm">{view.name}</span>
      {#if view.state !== null}<span class="st {view.tone}">{view.state}</span>{/if}
    </div>
    {#if view.auth !== null}
      <div class="kv"><span class="k">type</span><span class="v">{view.auth}</span></div>
    {/if}
    {#each view.windows as window (window.label)}
      <div class="bar">
        <span class="lb">{window.label}</span>
        <span class="tk"><span class="fl" style={`width:${window.percent}%`}></span></span>
        <span class="pc">{Math.round(window.percent)}%</span>
        {#if window.reset !== ''}<span class="eta">{window.reset}</span>{/if}
      </div>
    {/each}
    {#if view.spend !== null}
      <hr />
      <div class="kv"><span class="k">day</span><span class="v">{view.spend.daily}</span></div>
      <div class="kv"><span class="k">week</span><span class="v">{view.spend.weekly}</span></div>
      <div class="kv"><span class="k">month</span><span class="v">{view.spend.monthly}</span></div>
    {/if}
    {#if view.balance !== null}
      <hr />
      <div class="kv"><span class="k">balance</span><span class="v">{view.balance} left</span></div>
    {/if}
  </div>
</details>
