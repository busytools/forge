<script lang="ts">
  import Field from '../composer/Field.svelte';
  import ListEditor from '../components/ListEditor.svelte';
  import NumberField from '../components/NumberField.svelte';
  import Select from '../components/Select.svelte';
  import Toggle from '../components/Toggle.svelte';
  import type { SettingsEditSpec } from '../wire/settings';

  /**
   * One key's control, chosen by the edit kind the catalogue gave it. A kind
   * this client does not know draws as a note rather than a guess.
   */
  let {
    path,
    spec,
    value,
    onchange,
  }: {
    path: string;
    spec: SettingsEditSpec;
    value: unknown;
    onchange: (next: unknown) => void;
  } = $props();

  const text = $derived(typeof value === 'string' ? value : '');
  const number = $derived(typeof value === 'number' ? value : null);
  const boolean = $derived(value === true);
  const items = $derived(Array.isArray(value) ? value.map(String) : []);
  const choice = $derived(typeof value === 'string' ? value : null);
</script>

{#if spec.kind === 'string'}
  <Field
    editor="settings"
    element="input"
    value={text}
    aria={{ label: path }}
    oninput={(event: Event) => onchange((event.currentTarget as HTMLInputElement).value)}
  />
{:else if spec.kind === 'number'}
  <NumberField editor="settings" value={number} label={path} {onchange} />
{:else if spec.kind === 'bool'}
  <Toggle checked={boolean} label={path} {onchange} />
{:else if spec.kind === 'enum'}
  <Select value={choice} options={spec.allowed} label={path} unset={!spec.required} {onchange} />
{:else if spec.kind === 'list'}
  <ListEditor editor="settings" values={items} label={path} {onchange} />
{:else}
  <span class="settings-value settings-dim">not editable here</span>
{/if}
