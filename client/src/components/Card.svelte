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
   * that works and one that does not. Every door here takes both. A
   * modified Space is not the door's: Shift+Space is page-up and the
   * platform chords are the browser's, so the handler keeps its hands off
   * them. The handler rides the DOOR only - a card that opens nothing binds
   * no key at all.
   */
  function key(event: KeyboardEvent): void {
    if (href === null || event.key !== ' ') return;
    if (event.shiftKey || event.ctrlKey || event.metaKey || event.altKey) return;
    event.preventDefault();
    if (event.currentTarget instanceof HTMLElement) event.currentTarget.click();
  }

  const door = $derived(href !== null);
</script>

{#snippet content()}
  <div class="k">
    <span class="dot {dot[tone]}"></span>{title}
    {#if door}<Chevron />{/if}
  </div>
  <div class="v">{value}</div>
  <div class="sub">{detail}</div>
{/snippet}

<!-- One body, two elements: a door is a real `<a href>` rather than a
     dynamic element wearing a role, which is what a screen reader, the
     hover and the browser's own link handling all read. -->
{#if href === null}
  <div class={klass}>{@render content()}</div>
{:else}
  <a class={klass} {href} onkeydown={key}>{@render content()}</a>
{/if}
