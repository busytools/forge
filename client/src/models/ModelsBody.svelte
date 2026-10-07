<script lang="ts">
  import Brand from '../components/Brand.svelte';
  import type {
    BenchTarget,
    BenchTier,
    CatalogueRow,
    DictateModelsWire,
    ModelRole,
  } from '../wire/models';
  import {
    activateLine,
    activeSource,
    benchLine,
    benchRoleWord,
    benchTargets,
    candidateFacts,
    checkLine,
    comparison,
    entryUrl,
    inUseRowFacts,
    installLine,
    modelChip,
    recommendation,
    resultFacts,
    resultVerdict,
    resultWhen,
    roleWord,
    rowAction,
    search,
    speedLabel,
    tierWord,
    updateWhy,
    type FactPart,
  } from './view';

  /**
   * The models page as it is drawn, from one read.
   *
   * `wire` is required and has no default: the app's only input is the
   * server, and a page that fell back to bundled data is the failure the
   * standard names. What the page does about a wire it has not got yet - the
   * loading line, the refusal - is the route's, in `Models.svelte`.
   *
   * The actions are the route's too: this component draws the controls and
   * says which row was pressed, and never talks to the connection itself.
   */
  let {
    wire,
    oncheck,
    oninstall,
    onactivate,
    ondeactivate,
    onbench,
    onbenchstop,
    onarm,
    onupdate,
    updated = null,
    refusal = null,
    mark = null,
  }: {
    wire: DictateModelsWire;
    oncheck: () => void;
    oninstall: (variant: string) => void;
    onactivate: (file: string) => void;
    ondeactivate: (role: ModelRole) => void;
    onbench: (target: BenchTarget, tier: BenchTier) => void;
    onbenchstop: () => void;
    onarm: () => void;
    onupdate: (variant: string) => void;
    updated?: string | null;
    refusal?: string | null;
    mark?: string | null;
  } = $props();

  let query = $state('');

  // Filtering follows the box: the whole feed is already here, so a search
  // that waited for a button would be a second step over data this side
  // already holds.
  const results = $derived(search(wire.rows, query));
  const picks = $derived(wire.updates.filter((update) => recommendation(update) !== null).length);
  const line = $derived(checkLine(wire.check, picks, wire.enabled));
  const placeholder = $derived(
    `search ${wire.rows.length} variants - parakeet, granite, whisper, moonshine...`,
  );

  // One download and one activation run at a time (the core refuses a
  // second), so a control is disabled while either is in flight rather than
  // offered and then refused.
  const busy = $derived(
    wire.install.state === 'downloading' || wire.activate.state === 'activating',
  );
  const transcribing = $derived(wire.in_use.find((model) => model.role === 'transcribing') ?? null);
  const pinnedTranscribing = $derived(transcribing?.from.from === 'config');
  const download = $derived(installLine(wire.install));
  const building = $derived(activateLine(wire.activate));
  const benchState = $derived(wire.bench);
  const bench = $derived(benchLine(wire.bench));
  const targets = $derived(benchTargets(wire.in_use, wire.installed, wire.updates));
</script>

{#snippet facts(list: FactPart[])}
  {#each list as part, i (`${i}/${part.text}`)}{#if i > 0}{' \u{b7} '}{/if}{#if part.hl}<b
        >{part.text}</b
      >{:else}{part.text}{/if}{/each}
{/snippet}

{#snippet op(opline: {
  mark: string;
  title: string;
  detail: string | null;
  percent: number | null;
})}
  <div class="status {opline.mark === 'live' ? '' : opline.mark}" role="status">
    <span class="dot {opline.mark}"></span>
    <span class="t">{opline.title}</span>
    {#if opline.percent !== null}
      <progress class="bar" max="100" value={opline.percent} aria-label="download progress"
      ></progress>
    {/if}
    <span class="spacer"></span>
    {#if opline.detail !== null}<span class="when">{opline.detail}</span>{/if}
  </div>
{/snippet}

{#snippet control(entry: CatalogueRow)}
  {@const action = rowAction(entry, wire.installed, transcribing)}
  {#if action.do === 'install'}
    <button class="chip" type="button" disabled={busy} onclick={() => oninstall(entry.variant)}
      >{action.label}</button
    >
  {:else if action.do === 'activate'}
    <button class="chip" type="button" disabled={busy} onclick={() => onactivate(action.file)}
      >{action.label}</button
    >
  {:else if action.do === 'off'}
    <span class="chip">{action.label}</span>
  {/if}
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

  <!-- The page's own state: a refused action in the core's words, and
       whichever download or activation is in flight. One place, because both
       sections start the same work. -->
  {#if refusal !== null}
    <div class="status failed" role="alert">
      <span class="dot failed"></span>
      <span class="t">{refusal}</span>
    </div>
  {/if}
  {#if download !== null}{@render op(download)}{/if}
  {#if building !== null}{@render op(building)}{/if}
  {#if updated !== null}
    <div class="status" role="status">
      <span class="dot ok"></span>
      <span class="t">update completed</span>
      <span class="spacer"></span>
      <span class="when">{updated} is now the transcribing model</span>
    </div>
  {/if}

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
        {@const source = activeSource(model.from)}
        <div class="model">
          <span class="role">{roleWord(model.role)}</span>
          <div class="facts">
            <div class="nm">{model.file}</div>
            <div class="meta">{@render facts(factsOf.pinned)}</div>
            <div class="meta">{@render facts(factsOf.measured)}</div>
            <div class="meta src">{source}</div>
          </div>
          <div class="side">
            {#if model.from.from === 'installed'}
              <button
                class="chip"
                type="button"
                disabled={busy}
                onclick={() => ondeactivate(model.role)}>use the default</button
              >
            {/if}
            <span class="chip"><span class="dot {chip.mark}"></span>{chip.text}</span>
          </div>
        </div>
      {/each}
      {#if wire.models_dir !== null}
        <p class="note">
          files live in <code>{wire.models_dir}</code> &middot; a download is checked against the feed's
          own byte length before any load; these files publish no digest
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
      <div class="cmp-head">
        <span class="t">read against the {roleWord(update.role)} model in use</span>
        <span class="when">
          {speedLabel(update.current.speed_x)} and {update.current.fleurs_en_wer}% word error, on
          the feed's own FLEURS-en and m4-max rows
        </span>
      </div>
      <div class="cmp-wrap">
        <table class="cmp">
          <caption>{updateWhy()}</caption>
          <thead>
            <tr>
              <th scope="col">model</th>
              <th scope="col">speed</th>
              <th scope="col">error</th>
              <th scope="col">licence</th>
              <th scope="col">the rule</th>
              <th scope="col"><span class="sr">actions</span></th>
            </tr>
          </thead>
          <tbody>
            <tr class="base">
              <th scope="row">{update.file}</th>
              <td>{speedLabel(update.current.speed_x)}</td>
              <td>{update.current.fleurs_en_wer}%</td>
              <td colspan="2">this is what is running now</td>
              <td></td>
            </tr>
            {#each comparison(update) as row (row.variant)}
              <tr class:pick={row.recommended}>
                <th scope="row">{row.display_name}</th>
                <td>{row.speed ?? 'not measured'}</td>
                <td>{row.error ?? 'not measured'}</td>
                <td>{row.license ?? 'no licence on the feed'}</td>
                <td>{row.verdict}</td>
                <td>
                  {#if row.recommended}
                    {#if update.role === 'transcribing' && pinnedTranscribing}
                      <!-- A pinned role refuses the load by name, so the row
                         keeps the download and drops the update control. -->
                      {@render control(row.source)}
                    {:else}
                      <button
                        class="chip"
                        type="button"
                        disabled={busy}
                        onclick={() => onupdate(row.variant)}
                        title="download it if needed, then load it as the {roleWord(
                          update.role,
                        )} model">Update to this model</button
                      >
                    {/if}
                  {/if}
                </td>
              </tr>
            {/each}
          </tbody>
        </table>
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
                {@render control(entry)}
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
      Benchmark <span class="why">score a model on this machine's own recordings</span>
    </h2>

    {#if !wire.enabled}
      <p class="note">
        the bench runs only with <code>[dictate] enabled</code> set &middot; it scores a model on the
        takes forge has recorded here
      </p>
    {:else}
      {#if bench !== null}{@render op(bench)}{/if}

      {#if targets.length === 0}
        <p class="note">
          nothing here can be benched yet &middot; a model in use or installed lands in this list,
          and the run loads it from disk
        </p>
      {:else}
        <ul class="list" aria-label="Models a bench can run">
          {#each targets as row (`${row.target.role}/${row.target.file}`)}
            <li>
              <span class="bench-row">
                <span class="nm">{row.target.file}</span>
                <span class="col">{benchRoleWord(row.target.role)}</span>
                {#if row.current}<span class="chip">in use</span>{/if}
                {#if row.recommended}<span class="chip">recommended</span>{/if}
                {#if row.target.pinned}<span class="col">pinned by [dictate]</span>{/if}
              </span>
              {#if benchState.state === 'running' && benchState.target.file === row.target.file}
                <button class="chip" type="button" onclick={onbenchstop}>stop the bench</button>
              {:else}
                <button
                  class="chip"
                  type="button"
                  disabled={busy}
                  onclick={() => onbench(row.target, 'consensus')}>bench it</button
                >
                {#if wire.read_aloud.recorded}
                  <button
                    class="chip"
                    type="button"
                    disabled={busy}
                    onclick={() => onbench(row.target, 'read_aloud')}>score the read-aloud</button
                  >
                {/if}
              {/if}
            </li>
          {/each}
        </ul>
        <p class="note">
          the run scores the model's own words against the takes forge has saved here plus the repo
          fixtures &middot; term accuracy first, speed second - every number measured on this
          machine
        </p>
      {/if}

      {#if wire.read_aloud.armed}
        <div class="status" role="status">
          <span class="dot live"></span>
          <span class="t">armed: the next take you dictate becomes the read-aloud set</span>
          <span class="spacer"></span>
          <span class="when">read the passage below aloud, then stop the take</span>
          <span class="detail passage">{wire.read_aloud.passage}</span>
        </div>
      {:else if !wire.read_aloud.recorded}
        <div class="empty">
          <p class="t">the read-aloud set is not recorded yet</p>
          <p class="d">
            Read this passage aloud once, with the bench armed - it is the only corpus whose words
            are known, so it is the only one that can score term accuracy:
          </p>
          <p class="d passage">{wire.read_aloud.passage}</p>
          <button class="chip" type="button" disabled={busy} onclick={onarm}
            >record the passage next time I dictate</button
          >
        </div>
      {/if}

      {#each wire.results as result (`${result.target.role}/${result.target.file}/${result.tier}/${result.corpus.sha256}`)}
        <div class="status">
          <span class="dot ok"></span>
          <span class="t">{result.target.file}</span>
          <span class="when">{tierWord(result.tier)}</span>
          <span class="spacer"></span>
          {#if resultWhen(result) !== null}<span class="when">{resultWhen(result)}</span>{/if}
          <span class="detail">{@render facts(resultFacts(result))}</span>
          <span class="detail">{resultVerdict(result, wire.in_use, wire.results)}</span>
        </div>
      {/each}
    {/if}
  </section>
</main>
