<script lang="ts">
  // A `<use>` reference resolves against the document it is in, so the
  // sprite is inlined once per page. Hidden and out of flow: it is a
  // definition, not something the page draws.
  import { SPRITE } from './sprite';
</script>

<!--
  The sprite is a constant compiled from `icons.rs`, not input: no user or
  server value reaches it, and a `<use>` reference needs the symbols in the
  document, so the raw tag is the only way to inline it.
-->
<!-- eslint-disable-next-line svelte/no-at-html-tags -->
{@html SPRITE}
