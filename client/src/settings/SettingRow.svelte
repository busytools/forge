<script lang="ts">
  import ClassMark from '../components/ClassMark.svelte';
  import type { SettingsClass, SettingsEditSpec, SettingsValue } from '../wire/settings';
  import EditCell from './EditCell.svelte';
  import ValueCell from './ValueCell.svelte';

  /**
   * One setting line: the key and its class badge on the left, the value or
   * the control right-aligned to the card's edge. The badge is the page's one
   * statement of the key's class.
   *
   * A document row and a new entry's field draw through the same line, so the
   * props are the row's parts rather than a `SettingsKey`: an added field has
   * no class the catalogue gave it and no read value.
   */
  let {
    leaf,
    path,
    held = null,
    classLabel = '',
    required = false,
    bad = false,
    dirty = false,
    editing,
    spec = null,
    value = null,
    shown = null,
    builtIn = null,
    fieldId = undefined,
    onchange = () => {},
  }: {
    leaf: string;
    path: string;
    held?: SettingsClass | null;
    classLabel?: string;
    required?: boolean;
    bad?: boolean;
    dirty?: boolean;
    editing: boolean;
    spec?: SettingsEditSpec | null;
    /** The draft's value, drawn by the control. */
    value?: unknown;
    /** The loaded value, drawn in read mode. */
    shown?: SettingsValue | null;
    builtIn?: string | null;
    /** The value slot's id, where a new entry's first control takes focus. */
    fieldId?: string | undefined;
    onchange?: (next: unknown) => void;
  } = $props();
</script>

<div class="settings-setting" class:edit={editing} class:bad class:dirty>
  <span class="settings-setting-label">
    <code title={path}>{leaf}</code>
    <ClassMark {held} label={classLabel} />
    {#if editing && required}<span class="settings-required">required</span>{/if}
    {#if bad}<span class="settings-error">error</span>{/if}
  </span>
  <span class="settings-setting-value" id={fieldId}>
    {#if editing && spec !== null}
      <EditCell {path} {spec} {value} onchange={(next) => onchange(next)} />
    {:else if shown !== null}
      <ValueCell value={shown} {builtIn} />
    {/if}
  </span>
</div>
