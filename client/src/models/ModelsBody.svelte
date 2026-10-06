<script lang="ts">
  import Brand from '../components/Brand.svelte';
  import type { DictateModelsWire } from '../wire/models';
  import {
    candidateFacts,
    checkLine,
    inUseRowFacts,
    modelChip,
    roleWord,
    search,
    updateFacts,
    type FactPart,
  } from './view';

  /**
   * The models page as it is drawn, from one read.
   *
   * `wire` is required and has no default: the app's only input is the
   * server, and a page that fell back to bundled data is the failure the
   * standard names. What the page does about a wire it has not got yet - the
   * loading line, the refusal - is the route's, in `Models.svelte`.
   */
  let {
    wire,
    oncheck,
    mark = null,
  }: { wire: DictateModelsWire; oncheck: () => void; mark?: string | null } = $props();

  let query = $state('');
  let asked = $state('');

  const results = $derived(search(wire.rows, asked));
  const line = $derived(checkLine(wire.check, wire.updates.length, wire.enabled));
  const placeholder = $derived(
    `search ${wire.rows.length} variants - parakeet, granite, whisper, moonshine...`,
  );

  function findModels(event: SubmitEvent): void {
    event.preventDefault();
    asked = query;
  }
</script>

{#snippet facts(list: FactPart[])}
  {#each list as part, i (`${i}/${part.text}`)}{#if i > 0}{' \u{b7} '}{/if}{#if part.hl}<b
        >{part.text}</b
      >{:else}{part.text}{/if}{/each}
{/snippet}

<main class="wrap models">
  <header class="top">
    <div class="brand">
      <Brand name={mark} />
      <span class="word">forge</span>
    </div>
    <div class="title">
      <h1 class="t">Dictation models</h1>
      <span class="s">transcribing &middot; cleanup &middot; updates &middot; benchmarks</span>
    </div>
    <a class="back" href="/">&larr; home</a>
  </header>

  <section class="block">
    <h2 class="hd4">
      In use <span class="why">the models every take runs through, pinned by file and digest</span>
    </h2>

    {#if wire.in_use.length === 0}
      <!-- Dictation off is its own state: an empty list here would read as a
           page that broke rather than as a section nobody switched on. -->
      <div class="empty">
        <p class="t">dictation is off</p>
        <p class="d">
          <code>[dictate] enabled</code> is not set in <code>forge.toml</code>, so no models are
          loaded and a take has nothing to run through. Add it and restart forge.
        </p>
      </div>
    {:else}
      {#each wire.in_use as model (`${model.role}/${model.file}`)}
        {@const chip = modelChip(model.state)}
        {@const factsOf = inUseRowFacts(model)}
        <div class="model">
          <span class="role">{roleWord(model.role)}</span>
          <div class="facts">
            <div class="nm">{model.file}</div>
            <div class="meta">{@render facts(factsOf.pinned)}</div>
            <div class="meta">{@render facts(factsOf.measured)}</div>
          </div>
          <div class="side">
            <span class="chip"><span class="dot {chip.mark}"></span>{chip.text}</span>
          </div>
        </div>
      {/each}
      {#if wire.models_dir !== null}
        <p class="note">
          files live in <code>{wire.models_dir}</code> &middot; verified by size and sha-256 before any
          load
        </p>
      {/if}
    {/if}
  </section>

  <section class="block">
    <h2 class="hd4">
      Updates <span class="why"
        >the transcribe.cpp catalogue is the feed; a pinned file never moves on its own</span
      >
    </h2>

    <div class="status {line.mark === 'ok' ? '' : line.mark}" role="status">
      <span class="dot {line.mark}"></span>
      <span class="t">{line.title}</span>
      {#if line.when !== null}<span class="when">{line.when}</span>{/if}
      <span class="spacer"></span>
      {#if wire.enabled && wire.check.state !== 'checking'}
        <button class="chip" type="button" onclick={oncheck}>Check now</button>
      {/if}
      {#if line.detail !== null}<span class="detail">{line.detail}</span>{/if}
    </div>

    {#each wire.updates as update (`${update.role}/${update.file}`)}
      <div class="status warn">
        <span class="dot warn"></span>
        <span class="t">{update.candidate.display_name}</span>
        <span class="when">for {roleWord(update.role)}</span>
        <span class="detail">{@render facts(updateFacts(update))}</span>
      </div>
    {/each}

    <p class="note">
      an adoption is never silent: a better model becomes a pull request with this page's numbers
      beside it, and you merge it
    </p>
  </section>

  <section class="block">
    <h2 class="hd4">
      Find a model <span class="why"
        >reads the catalogue the runtime already publishes, from this machine alone</span
      >
    </h2>

    {#if !wire.enabled}
      <!-- The feed is read only while the section is on, so an off forge has
           no catalogue at all: a search box here would answer every query
           with "no entry matches", which reads as a feed that found nothing
           rather than one that was never fetched. -->
      <p class="note">
        the feed is read only with <code>[dictate] enabled</code> set &middot; switch it on and restart
        forge, and the catalogue loads here
      </p>
    {:else}
      <form class="searchbar" onsubmit={findModels}>
        <!-- `nowhere` is the editors table's name for a box a take's words are
             not routed to: this one takes typing, and the reader's dictation
             stays where it was. -->
        <input
          type="search"
          bind:value={query}
          data-editor="nowhere"
          aria-label="Search the model catalogue"
          {placeholder}
        />
        <button class="chip go" type="submit">Search</button>
      </form>

      {#if asked === ''}
        <p class="note">
          nothing searched yet &middot; the feed's whole catalogue is already here, so a search
          reads nothing off the network
        </p>
      {:else if results.length === 0}
        <p class="note">no entry matches <code>{asked}</code> &middot; try a family name</p>
      {:else}
        <ul class="list" aria-label="Catalogue results">
          {#each results as entry (entry.variant)}
            {@const face = candidateFacts(entry)}
            <li class="cand">
              <span class="nm">{entry.variant}</span>
              <span class="col">{@render facts(face.spec)}</span>
              <span class="col">{@render facts(face.kind)}</span>
            </li>
          {/each}
        </ul>
        <p class="note">size, speed and error are the catalogue's own m4-max measurements</p>
      {/if}
    {/if}
  </section>

  <section class="block">
    <h2 class="hd4">
      Benchmark <span class="why">your corpus, your mic: term accuracy first, speed second</span>
    </h2>

    <!-- The RUN is its own piece of work: the corpus it scores against is the
         takes this machine has recorded plus the read-aloud set, and none of it
         exists yet. No control here names a run that cannot start. -->
    <div class="empty">
      <p class="t">no benchmark has run on this machine yet</p>
      <p class="d">
        The bench will take the takes forge has recorded here, plus the read-aloud set, and score
        each candidate on your own words. Until it lands, the numbers above are the catalogue's
        measurements and not this machine's.
      </p>
    </div>
  </section>
</main>
