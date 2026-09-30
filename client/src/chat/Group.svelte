<script lang="ts">
  import Call from './Call.svelte';
  import { iconOf, type CallStatus } from './families';
  import GroupShell from './GroupShell.svelte';
  import { opensByDefault, type ToolLeaf } from './leaves';
  import type { FamilyLeaves } from './units';

  /**
   * One run of consecutive calls, drawn in the shared group shell: a lane per
   * family, with each call on its own row behind a rail.
   */
  let { families, status }: { families: FamilyLeaves[]; status: CallStatus } = $props();

  const calls = $derived(families.reduce((total, family) => total + family.calls.length, 0));

  /**
   * What names the run, which is what a view keys its open state on.
   *
   * The call the run OPENED with: the calls after it are appended, so the
   * thing that identifies the run does not move as they arrive.
   */
  const key = $derived(`group-${families[0]?.calls[0]?.id ?? 'empty'}`);

  const lanes = $derived(
    families.map((family) => ({
      key: family.label,
      glyph: iconOf(family.row),
      label: family.label,
      rows: family.calls,
    })),
  );

  /** Whether a call's body is drawn without being asked for: a mutation's diff. */
  function opens(call: ToolLeaf): boolean {
    return opensByDefault(call.name);
  }
</script>

{#snippet callLeaf(leaf: ToolLeaf)}
  <Call call={leaf} open={opens(leaf)} />
{/snippet}

<GroupShell name={key} count={calls} noun="tool call" {status} {lanes} leaf={callLeaf} />
