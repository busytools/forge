<script lang="ts">
  import Icon from '../components/Icon.svelte';
  import { HEADINGS, LIST_ID, mark, rowId, type Offer } from './autocomplete';

  let {
    offer,
    marked = 0,
    onpick,
  }: {
    offer: Offer;
    /** Which row a key would take, which is the first until one moves it. */
    marked?: number;
    onpick: (at: number) => void;
  } = $props();

  const heading = $derived(HEADINGS[offer.kind]);
</script>

<div class="ac">
  <div class="h">
    <Icon name={heading.icon} />
    {heading.title}
    <!-- The count is the number of matches, so a list the window cuts short
         never reads as the whole list. -->
    <span class="n">{offer.total}</span>
  </div>
  <div
    class="rows"
    id={LIST_ID}
    role="listbox"
    aria-label={heading.title}
    tabindex="-1"
    aria-activedescendant={rowId(offer, marked)}
  >
    <!-- One group per heading, which is the shape a listbox's groups take: an
         unlabelled one when the list heads nothing, which is every list but
         the `/` list's whole one. -->
    {#each offer.groups as group (group.from)}
      <div role="group" aria-label={group.title}>
        {#if group.title !== null}
          <div class="grp">{group.title}</div>
        {/if}
        {#each group.rows as row, at (row.insert)}
          {@const index = group.from + at}
          {@const parts = mark(row.text, offer.query)}
          <div
            class="it"
            class:sel={index === marked}
            role="option"
            id={rowId(offer, index)}
            aria-selected={index === marked}
            tabindex="-1"
            onclick={() => onpick(index)}
            onkeydown={(event) => {
              if (event.key !== 'Enter' && event.key !== ' ') return;
              event.preventDefault();
              onpick(index);
            }}
          >
            {#if row.glyph !== null}
              <span class="g">{row.glyph}</span>
            {/if}
            <span class="p"
              >{parts.before}{#if parts.hit !== ''}<em>{parts.hit}</em>{/if}{parts.after}</span
            >
            {#if row.detail !== ''}
              <span class="d">{row.detail}</span>
            {/if}
          </div>
        {/each}
      </div>
    {/each}
  </div>
  <div class="keys">
    <span><kbd>↑</kbd><kbd>↓</kbd> select</span>
    <span><kbd>Enter</kbd> choose</span>
    <span><kbd>Esc</kbd> close</span>
  </div>
</div>
