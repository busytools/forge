<script lang="ts">
  import Icon from '../../components/Icon.svelte';
  import type { MonitorRecord } from '../wire';
  import { monitorsSection } from '../view';
  import Section from './Section.svelte';

  /**
   * The monitors section. Monitors live here and not in the chat, so this is
   * the only surface that says what a session is watching.
   *
   * Every card draws its command. The output under it is not here: a card
   * draws the watched file's tail, and the tail is read off the machine
   * running forge - the record names the file, and this client does not read
   * the server's disk.
   */
  let { monitors, now }: { monitors: MonitorRecord[]; now: number } = $props();

  const view = $derived(monitorsSection(monitors, now));
</script>

<Section name="monitors" icon="monitors" summary={view.summary}>
  {#each view.rows as monitor (monitor.name + monitor.command)}
    <div class="sa">
      <div class="sh">
        {#if monitor.running}
          <span class="st"><span class="ring" style="width:8px;height:8px"></span></span>
        {:else}
          <Icon name="check" class="st" />
        {/if}
        <Icon name="monitors" class="gl" />
        <span class="nm">{monitor.name}</span>
        <span class="n">{monitor.label}</span>
      </div>
      <div class="tt"><span class="tg">$</span> {monitor.command}</div>
    </div>
  {/each}
</Section>
