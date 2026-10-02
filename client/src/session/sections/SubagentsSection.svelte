<script lang="ts">
  import Section from './Section.svelte';

  /**
   * The subagents section, which is the only surface subagents have - the
   * chat suppresses them.
   *
   * **The instances are not on the socket, so this draws its own gap rather
   * than a card.** A card is one instance with its calls under it, folded from
   * the conversation the server holds; `subagents` on the session record is
   * the catalogue of agent TYPES the CLI offers, and a page of history carries
   * no instance either. The section is drawn when the record says the seat
   * dispatched, because an absent section reads as "no sub-agents ran" - the
   * same mistake, one section along, as drawing a healthy state for an
   * unknown one.
   */
  let { dispatches }: { dispatches: boolean } = $props();
</script>

{#if dispatches}
  <Section name="subagents" icon="subagents">
    <div class="note">
      This seat has dispatched sub-agents. The socket does not carry their instances yet, so the
      calls under each one cannot be drawn here.
    </div>
  </Section>
{/if}
