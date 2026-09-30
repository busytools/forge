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
   * What names the run, which is the handle its row carries.
   *
   * The call the run OPENED with: the calls after it are appended, so the
   * thing that identifies the run does not move as they arrive.
   */
  const key = $derived(`group-${families[0]?.calls[0]?.id ?? 'empty'}`);

  const lanes = $derived(
    families.map((family) => ({
      // A label is not an identity: the fold draws a family as `(label, row
      // kind)`, and a built-in beside an MCP server named after it - a `Read`
      // and an `mcp__read__query` - is two families with one word. Keying a
      // lane by the word alone is a duplicate key, and a duplicate key stops
      // the whole turn drawing at mount.
      key: `${family.label}-${family.row.kind}`,
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
