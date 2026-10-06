<script lang="ts">
  import Brand from '../components/Brand.svelte';
  import type { DictateModelsWire } from '../wire/models';
  import {
    candidateFacts,
    checkLine,
    entryUrl,
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

  // Filtering follows the box: the whole feed is already here, so a search
  // that waited for a button would be a second step over data this side
  // already holds.
  const results = $derived(search(wire.rows, query));
  const line = $derived(checkLine(wire.check, wire.updates.length, wire.enabled));
  const placeholder = $derived(
    `search ${wire.rows.length} variants - parakeet, granite, whisper, moonshine...`,
  );
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
    <h2 class="hd4">In use <span class="why">what dictation runs on this machine today</span></h2>

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
      Updates <span class="why">whether a better model is on the feed</span>
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
        <span class="when">replaces the {roleWord(update.role)} model</span>
        <span class="detail">
          {@render facts(updateFacts(update))}
        </span>
        <span class="detail">
          Faster and more accurate than the model in use, on the feed's own test set. Taking it
          means pinning it here and opening a pull request - the bench that checks a candidate on
          your own recordings is not built yet.
        </span>
      </div>
    {/each}
  </section>

  <section class="block">
    <h2 class="hd4">
      Find a model <span class="why">search the catalogue the runtime publishes</span>
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
      <!-- `nowhere` is the editors table's name for a box a take's words are
           not routed to: this one takes typing, and the reader's dictation
           stays where it was. -->
      <input
        class="find"
        type="search"
        bind:value={query}
        data-editor="nowhere"
        aria-label="Search the model catalogue"
        {placeholder}
      />

      <div aria-live="polite">
        {#if query.trim() === ''}
          <p class="note">
            type a name &middot; the feed's whole catalogue is already here, so this reads nothing
            off the network
          </p>
        {:else if results.length === 0}
          <p class="note">no entry matches <code>{query}</code> &middot; try a family name</p>
        {:else}
          <p class="note">
            {results.length} of {wire.rows.length} entries &middot; each opens its catalogue entry
          </p>
          <ul class="list" aria-label="Catalogue results">
            {#each results as entry (entry.variant)}
              {@const face = candidateFacts(entry)}
              <li>
                <a
                  class="cand"
                  href={entryUrl(entry.variant)}
                  target="_blank"
                  rel="noreferrer"
                  title="the catalogue entry, on github"
                >
                  <span class="nm">{entry.variant}</span>
                  <span class="col">{@render facts(face.spec)}</span>
                  <span class="col">{@render facts(face.kind)}</span>
                  <span class="go" aria-hidden="true">&#8599;</span>
                </a>
              </li>
            {/each}
          </ul>
          <p class="note">size, speed and error are the catalogue's own m4-max measurements</p>
        {/if}
      </div>
    {/if}
  </section>

  <section class="block">
    <h2 class="hd4">
      Benchmark <span class="why">score a model on your own recordings</span>
    </h2>

    <!-- The RUN is its own piece of work: the corpus it scores against is the
         takes this machine has recorded plus the read-aloud set, and none of it
         exists yet. No control here names a run that cannot start. -->
    <div class="empty">
      <p class="t">the benchmark is not built yet</p>
      <p class="d">
        It will score each candidate on the takes forge has already recorded here, plus a read-aloud
        set - term accuracy first, speed second - and that score is what decides an update. Until it
        lands, every number on this page is the feed's own, measured on an m4 max and not on your
        machine.
      </p>
    </div>
  </section>
</main>
