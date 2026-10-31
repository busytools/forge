<script lang="ts">
  /**
   * One choice from a closed set. `unset` is offered as its own option when
   * the key is optional, so clearing an enum writes nothing rather than a
   * name that is not one of its values.
   */
  let {
    value,
    options,
    label,
    unset = false,
    onchange,
  }: {
    value: string | null;
    options: string[];
    label: string;
    unset?: boolean;
    onchange: (next: string | null) => void;
  } = $props();
</script>

<select
  class="select"
  aria-label={label}
  value={value ?? ''}
  onchange={(event) => {
    const next = event.currentTarget.value;
    onchange(next === '' ? null : next);
  }}
>
  {#if unset}
    <option value="">not set</option>
  {/if}
  {#each options as option (option)}
    <option value={option}>{option}</option>
  {/each}
</select>
