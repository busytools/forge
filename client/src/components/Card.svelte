<script lang="ts">
  import type { BandCard, Tone } from '../home/view';
  import Chevron from './Chevron.svelte';

  /**
   * One card in the system band. Quiet until it is not.
   *
   * A card with an `href` is a way in rather than a read: the whole card is
   * the target, and the chevron on its title line is what says so at rest -
   * an affordance only the pointer uncovers is what the standard forbids.
   */
  let { title, tone, value, detail, href = null }: BandCard = $props();

  const dot: Record<Tone, string> = { ready: 'ok', warn: 'warn', bad: 'bad', off: 'off' };
  // `ready` is the quiet card and carries no modifier, which is the state a
  // plain `.svc` draws.
  const klass = $derived(tone === 'ready' ? 'svc' : `svc ${tone}`);

  /**
   * Space activates a card that is a way in, as Enter does.
   *
   * Only Enter is a link's own key: a browser scrolls on Space over an
   * anchor, so a keyboard reader reaching a door in the band has one key
   * that works and one that does not. Every door here takes both.
   */
  function key(event: KeyboardEvent): void {
    if (href === null || event.key !== ' ') return;
    event.preventDefault();
    if (event.currentTarget instanceof HTMLElement) event.currentTarget.click();
  }
</script>

<svelte:element
  this={href === null ? 'div' : 'a'}
  class={klass}
  href={href ?? undefined}
  role={href === null ? undefined : 'link'}
  onkeydown={key}
>
  <div class="k">
    <span class="dot {dot[tone]}"></span>{title}
    {#if href !== null}<Chevron />{/if}
  </div>
  <div class="v">{value}</div>
  <div class="sub">{detail}</div>
</svelte:element>
