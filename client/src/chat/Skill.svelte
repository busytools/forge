<script lang="ts">
  import Chevron from '../components/Chevron.svelte';
  import Icon from '../components/Icon.svelte';
  import Prose from './Prose.svelte';

  /**
   * A skill the CLI loaded into the conversation.
   *
   * **The body arrives as the reader's own user frame**, and drawn that way it
   * wore an attribution nobody earned - the injected text is the skill, not
   * words someone typed. So the row names the skill and holds the whole body
   * behind its own open: collapsed it says which skill loaded, opened it reads
   * as the skill's own markdown, preamble line dropped.
   */
  let { name, body }: { name: string; body: string } = $props();
</script>

<details class="leaf">
  <summary>
    <Icon name="skill" class="st sk" />
    <span class="tn">skill /{name}</span>
    <Chevron />
  </summary>
  <div class="body">
    <Prose text={body} />
  </div>
</details>
