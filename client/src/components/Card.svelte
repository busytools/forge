<script lang="ts">
  import type { BandCard, Tone } from '../home/view';

  /** One card in the system band. Quiet until it is not. */
  let { title, tone, value, detail }: BandCard = $props();

  const dot: Record<Tone, string> = { ready: 'ok', warn: 'warn', bad: 'bad', off: 'off' };
  // `ready` is the quiet card and carries no modifier, which is the state a
  // plain `.svc` draws.
  const klass = $derived(tone === 'ready' ? 'svc' : `svc ${tone}`);
</script>

<div class={klass}>
  <div class="k"><span class="dot {dot[tone]}"></span>{title}</div>
  <div class="v">{value}</div>
  <div class="sub">{detail}</div>
</div>
