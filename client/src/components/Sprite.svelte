<script lang="ts">
  // A `<use>` reference resolves against the document it is in, so the
  // sprite is inlined once per page. Hidden and out of flow: it is a
  // definition, not something the page draws.
  import { SPRITE } from './sprite';
</script>

{@html SPRITE}
