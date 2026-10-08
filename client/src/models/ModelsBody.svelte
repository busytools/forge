<script lang="ts">
  import Brand from '../components/Brand.svelte';
  import { FALLBACK_FLOOR_DB, fractionOf, meterWindow } from '../composer/meter';
  import GroupFold from '../session/GroupFold.svelte';
  import type { SetRecorder } from './recorder.svelte';
  import type {
    BenchResult,
    BenchTarget,
    BenchTier,
    CatalogueRow,
    DictateModelsWire,
    ModelRole,
    ReadAloudRecording,
  } from '../wire/models';
  import {
    activateLine,
    activeSource,
    benchLine,
    benchRoleWord,
    benchTargets,
    candidateFacts,
    checkLine,
    cleanupCandidates,
    cleanupPick as cleanupPickOf,
    comparison,
    entryUrl,
    families,
    fastest,
    inUseLicense,
    inUseRowFacts,
    installLine,
    modelChip,
    recommendation,
    recordingFacts,
    recordingLength,
    resultFacts,
    resultHeadline,
    resultVerdict,
    resultWhen,
    roleWord,
    rowAction,
    search,
    sizeLabel,
    speedLabel,
    sweepCost,
    sweepHeadline,
    sweepPlan,
    sweepScope,
    tierWord,
    updateFacts,
    updateWhy,
    type FactPart,
    type SweepPlan,
    type SweepVerdict,
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
    onrecord,
    onrecordstop,
    onrecorddelete,
    recorder = null,
    recordingLine = null,
    onbenchdelete,
    onupdate,
    updated = null,
    refusal = null,
    mark = null,
    sweep = null,
    sweepLine = null,
    verdicts = [],
    onsweep,
    onsweepcancel,
    onadopt,
    onuninstall,
  }: {
    wire: DictateModelsWire;
    oncheck: () => void;
    oninstall: (variant: string) => void;
    onactivate: (file: string, role: ModelRole) => void;
    ondeactivate: (role: ModelRole) => void;
    onbench: (target: BenchTarget, tier: BenchTier) => void;
    onbenchstop: () => void;
    /** Begin recording the read-aloud passage. */
    onrecord: () => void;
    /** End it: `keep` saves the recording as the set. */
    onrecordstop: (keep: boolean) => void;
    /** Drop one recording from the set. */
    onrecorddelete: (recording: ReadAloudRecording) => void;
    /** This side's capture, while one runs; `null` when none is. The card
     * reads its frames and its levels and nothing else. */
    recorder?: Pick<SetRecorder, 'wire'> | null;
    /** This side's own line about the recording - a refused microphone, a
     * keep that never reached the socket. */
    recordingLine?: string | null;
    onbenchdelete: (result: BenchResult) => void;
    onupdate: (variant: string) => void;
    updated?: { file: string; role: ModelRole } | null;
    refusal?: string | null;
    mark?: string | null;
    /** The sweep in flight, or `null` when none is: the card prices the
     * press before it spends, and says where it is while it runs. */
    sweep?: SweepPlan | null;
    /** Where the sweep is, in its own words. */
    sweepLine?: string | null;
    /** The last sweep's verdicts, kept after the chain clears. */
    verdicts?: SweepVerdict[];
    /** Press the one button: score the picks on this machine. */
    onsweep: () => void;
    /** Stop the sweep where it is. */
    onsweepcancel: () => void;
    /** Take a verdict's winner into the role it read best in. */
    onadopt: (variant: string, role: ModelRole) => void;
    /** Remove one downloaded model - the file and its record. Refused by the
     * core while a role runs it, in the core's own words. */
    onuninstall: (file: string) => void;
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
  const targets = $derived(benchTargets(wire.in_use, wire.installed, wire.updates, wire.rows));

  /**
   * The role whose proposal the Updates section draws. The rows in use are
   * the selector; until one is pressed the section follows the first role
   * that has something to propose, so a role with news is never hidden
   * behind a role without any.
   */
  let selected = $state<ModelRole | null>(null);
  const shownRole = $derived(selected ?? wire.updates[0]?.role ?? 'transcribing');
  const shown = $derived(wire.updates.filter((update) => update.role === shownRole));

  /** The cleanup role's candidates, and the best of the runs on them. */
  const cleanupRows = $derived(cleanupCandidates(wire.rows, wire.installed, wire.results));
  const cleanupPick = $derived(cleanupPickOf(cleanupRows));
  /** The model the cleanup role runs, which the pick is read against. */
  const inUseCleanup = $derived(
    wire.in_use.find((model) => model.role === 'normalization') ?? null,
  );

  /** The check line's own news, per role: the feed proposes for the
   * transcribing role and the bench decides the cleanup one, so each half is
   * said by the thing that actually decides it - and both are said without a
   * press, because a reader should not have to run a check to learn there is
   * one. */
  const roleNews = $derived.by(() => {
    const proposed = wire.updates.some((update) => recommendation(update) !== null);
    const transcribing = proposed ? 'transcribing has an update' : 'transcribing is up to date';
    if (cleanupPick === null) {
      return `${transcribing} \u{b7} cleanup has nothing measured yet`;
    }
    if (cleanupPick.candidate.installed?.file === inUseCleanup?.file) {
      return `${transcribing} \u{b7} cleanup is on the model its bench picked`;
    }
    return `${transcribing} \u{b7} cleanup: ${cleanupPick.candidate.row.variant} read best in your bench`;
  });

  /** The material a bench scores on, in the line the section opens with: the
   * takes the last run over them scored, and the recordings this machine
   * holds. Both are counts off the read, never a promise. */
  const fixturesLine = $derived.by(() => {
    const last = wire.results.find((result) => result.tier === 'consensus');
    const takes =
      last === undefined
        ? 'no run has scored the takes here yet'
        : `the last run scored ${last.corpus.clips} takes`;
    const held = wire.read_aloud.recordings.length;
    const recordings =
      held === 0
        ? 'no read-aloud recording'
        : `${held} read-aloud ${held === 1 ? 'recording' : 'recordings'}`;
    return `fixtures \u{b7} ${takes} \u{b7} ${recordings}`;
  });

  /** What a press would do right now, priced from this read: the card says
   * what will run, what it costs and what it scores on, before it spends. */
  const nextSweep = $derived(sweepPlan(wire));
  const sweepCorpus = $derived(
    nextSweep.tier === 'read_aloud' ? 'your read-aloud set' : 'your takes',
  );
  const sweepShape = $derived.by(() => {
    const plan = nextSweep;
    const candidates = plan.runs.filter((run) => run.why === 'candidate').length;
    const picks: string[] = [];
    if (plan.runs.some((run) => run.why === 'pick')) picks.push("the feed's transcribing pick");
    if (candidates > 0) {
      const of = plan.beyond > 0 ? ` of ${candidates + plan.beyond}` : '';
      picks.push(`the ${candidates} most-downloaded cleanup candidates${of}`);
    }
    if (picks.length === 0) {
      return 'nothing to score yet - the feed proposes no transcribing update and lists no cleanup candidates';
    }
    return picks.join(' + ');
  });
  const sweepPrice = $derived.by(() => {
    const parts = [`up to ${nextSweep.runs.length} runs over ${sweepCorpus}`];
    parts.push(
      nextSweep.bytes === 0
        ? 'no download needed'
        : `about ${sizeLabel(nextSweep.bytes)} to download`,
    );
    parts.push(
      nextSweep.seconds_runs === null
        ? 'no run on this corpus yet, so no clock to estimate by'
        : `the last run over ${sweepCorpus} took ${duration(nextSweep.seconds_runs)}`,
    );
    parts.push('a run already measured on that corpus is kept, not repeated');
    return parts.join(' \u{b7} ');
  });

  /** A span of seconds as the card reads it. */
  function duration(seconds: number): string {
    const whole = Math.round(seconds);
    if (whole < 1) return '<1s';
    const minutes = Math.floor(whole / 60);
    return minutes > 0 ? `${minutes}m ${whole % 60}s` : `${whole}s`;
  }

  /** What switching to a verdict's winner would cost now. */
  function adoptCost(verdict: SweepVerdict): number {
    return sweepCost(wire, verdict.best.run.variant);
  }

  /** How many level readings the recording card draws. */
  const REC_CELLS = 40;

  /**
   * The recording's own clock, off the frames it has produced - the ring's
   * counters are the signals, so the card repaints as the recording moves.
   * Fifty frames is one second at the 20 ms cadence.
   */
  const recClock = $derived.by(() => {
    const seconds = Math.floor((recorder?.wire.frames ?? 0) / 50);
    return `${Math.floor(seconds / 60)}:${String(seconds % 60).padStart(2, '0')}`;
  });

  /** The recording's newest levels, at the card's own pitch. */
  const recCells = $derived.by(() => {
    // The frame count is the tick: `dbfs` is read off the ring rather than
    // held as a signal, so this recomputes with the count.
    void recorder?.wire.frames;
    const levels = (recorder?.wire.dbfs ?? []).map((peakDb) =>
      fractionOf(peakDb, FALLBACK_FLOOR_DB),
    );
    return meterWindow(levels, REC_CELLS);
  });

  // What a reader can browse before they know a name: the feed's own
  // families, and its fastest entries.
  const familyList = $derived(families(wire.rows));
  const fastestRows = $derived(fastest(wire.rows, 3));

  /**
   * The failure a reader has closed. Keyed by what failed, so a new failure
   * - another model, another reason - draws rather than staying hidden
   * behind a dismissal that was about something else. The bench state itself
   * lives in the core and is not this side's to clear.
   */
  let dismissedFailure = $state<string | null>(null);
  const failureKey = $derived(
    wire.bench.state === 'failed' ? `${wire.bench.target.file}|${wire.bench.reason}` : null,
  );
  const failureShown = $derived(failureKey !== null && dismissedFailure !== failureKey);
</script>

{#snippet facts(list: FactPart[])}
  {#each list as part, i (`${i}/${part.text}`)}{#if i > 0}{' \u{b7} '}{/if}{#if part.hl}<b
        >{part.text}</b
      >{:else}{part.text}{/if}{/each}
{/snippet}

{#snippet op(
  opline: {
    mark: string;
    title: string;
    detail: string | null;
    percent: number | null;
  },
  ondismiss: (() => void) | null = null,
)}
  <div class="status {opline.mark === 'live' ? '' : opline.mark}" role="status">
    <span class="dot {opline.mark}"></span>
    <span class="t">{opline.title}</span>
    {#if opline.percent !== null}
      <progress class="bar" max="100" value={opline.percent} aria-label="download progress"
      ></progress>
    {/if}
    <span class="spacer"></span>
    {#if ondismiss !== null}
      <button class="chip" type="button" aria-label="close this" onclick={ondismiss}>close</button>
    {/if}
    {#if opline.detail !== null}<span class="when">{opline.detail}</span>{/if}
  </div>
{/snippet}

{#snippet control(entry: CatalogueRow)}
  {@const action = rowAction(entry, wire.installed, wire.in_use)}
  {#if action.do === 'install'}
    <button class="chip" type="button" disabled={busy} onclick={() => oninstall(entry.variant)}
      >{action.label}</button
    >
  {:else if action.do === 'activate'}
    <button
      class="chip"
      type="button"
      disabled={busy}
      onclick={() => onactivate(action.file, action.role)}>{action.label}</button
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
      <span class="when">{updated.file} is now the {roleWord(updated.role)} model</span>
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
        <div class="model" class:shown={shownRole === model.role}>
          <!-- **The whole row is the selector.** A card that only answers on
               one word reads as furniture; this one takes the press anywhere
               on it and answers like a button - the ground under the pointer,
               a focus ring, and the pressed state on its edge. The row also
               carries a control of its own, so the hit target is an overlay
               rather than the card itself: a button inside a button is not
               HTML. -->
          <button
            class="pick"
            type="button"
            aria-pressed={shownRole === model.role}
            aria-label="show the {roleWord(model.role)} updates"
            title="show the {roleWord(model.role)} updates"
            onclick={() => (selected = model.role)}
          ></button>
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
      {#if wire.enabled}<span class="detail">{roleNews}</span>{/if}
      {#if line.detail !== null}<span class="detail">{line.detail}</span>{/if}
    </div>

    {#if wire.enabled}
      <p class="note">
        a check reads the catalogue the transcribe.cpp runtime publishes - every variant with its
        sizes, licences, and the speeds and error rates its maintainers measured - and compares it
        with the models pinned here. It measures nothing on this machine.
      </p>
    {/if}

    {#if shownRole === 'normalization'}
      <!-- The cleanup role's own view: no feed publishes speed or error for
           a normalizer, so the bench decides. The candidates are the Hub's
           rows, each carrying this machine's own runs. -->
      <p class="note">
        the feed publishes no speed or error for a normalizer &middot; the bench decides this role:
        the pick is the best of your own runs on one corpus, and a candidate with no run yet offers
        the bench
      </p>
      {#if cleanupPick !== null}
        <div class="status">
          <span class="dot ok"></span>
          <span class="t">{cleanupPick.candidate.row.variant}</span>
          <span class="when">measured best on {tierWord(cleanupPick.result.tier)}</span>
          <span class="spacer"></span>
          {#if cleanupPick.candidate.installed?.file !== inUseCleanup?.file}
            <button
              class="chip"
              type="button"
              disabled={busy}
              onclick={() => onadopt(cleanupPick.candidate.row.variant, 'normalization')}
              >switch to it</button
            >
          {/if}
          <span class="detail">{@render facts(resultFacts(cleanupPick.result))}</span>
        </div>
      {:else if cleanupRows.length > 0}
        <p class="note">
          nothing here is benched twice over one corpus with words known to be true &middot; the
          takes tier reads speed and agreement and a faster normalizer is not a better one, so the
          pick waits for the read-aloud tier &middot; a run lands its numbers on the candidate's row
        </p>
      {/if}
      {#if cleanupRows.length === 0}
        <p class="note">
          the cleanup feed listed nothing this machine would run &middot; its candidates are English
          normalizers with a quant llama.cpp loads and a fetched count behind them
        </p>
      {:else}
        <ul class="list" aria-label="Cleanup candidates">
          {#each cleanupRows as candidate (candidate.row.variant)}
            <li>
              <a
                class="cand"
                href={entryUrl(candidate.row)}
                target="_blank"
                rel="noreferrer"
                title="the catalog entry, on hugging face"
              >
                <span class="nm">{candidate.row.variant}</span>
                <span class="col">{@render facts(candidateFacts(candidate.row).spec)}</span>
                <span class="go" aria-hidden="true">&#8599;</span>
              </a>
              {@render control(candidate.row)}
            </li>
            {#each candidate.results as result (`${result.tier}/${result.corpus.sha256}`)}
              <!-- The run belongs to the candidate above it, so it sits
                   inside that row rather than between two of them: a line of
                   numbers with no name reads as nobody's. -->
              <li class="run">
                <span class="facts">
                  {tierWord(result.tier)} &middot; {@render facts(resultHeadline(result))} &middot; {resultVerdict(
                    result,
                    wire.in_use,
                    wire.results,
                  )}
                </span>
              </li>
            {/each}
          {/each}
        </ul>
      {/if}
    {:else}
      {#each shown as update (`${update.role}/${update.file}`)}
        {@const proposal = recommendation(update)}
        <!-- The recommendation first, and actionable: a line that says there
             is an update has to show the update. The table under it is the
             rule's own working - evidence for the pick, not the pick - so it
             folds, and the row that carried the control keeps only its
             verdict. -->
        {#if proposal !== null}
          <div class="status warn">
            <span class="dot warn"></span>
            <span class="t">{proposal.row.display_name}</span>
            <span class="when">replaces the {roleWord(update.role)} model</span>
            <span class="spacer"></span>
            {#if update.role === 'transcribing' && pinnedTranscribing}
              <!-- A pinned role refuses the load by name, so the row keeps the
                   download and drops the update control. -->
              {@render control(proposal.row)}
            {:else}
              <button
                class="chip"
                type="button"
                disabled={busy}
                onclick={() => onupdate(proposal.row.variant)}
                title="download it if needed, then load it as the {roleWord(update.role)} model"
                >Update to this model</button
              >
            {/if}
            <span class="detail">{@render facts(updateFacts(update, proposal.row))}</span>
            <span class="detail">
              Proposed because it beats the model in use on both of the feed's own measurements -
              fewer errors on its English test set, and a faster realtime factor on an m4-max - and
              its licence is not marked non-commercial. Taking it downloads it here if it is not
              already, and loads it as the {roleWord(update.role)} model.
            </span>
          </div>
        {:else}
          <p class="note">
            no update for {roleWord(update.role)} &middot; nothing the feed measured beats the model in
            use on both axes
          </p>
        {/if}

        <GroupFold
          heading="read against the {roleWord(update.role)} model in use"
          count={update.candidates.length}
        >
          <div class="cmp-head">
            <span class="when">
              {speedLabel(update.current.speed_x)} and {update.current.fleurs_en_wer}% word error,
              on the feed's own FLEURS-en and m4-max rows
            </span>
          </div>
          {#if update.candidates.length === 0}
            <p class="note">
              no other English model in the feed is measured on both axes &middot; there is nothing
              to compare against
            </p>
          {/if}
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
                </tr>
              </thead>
              <tbody>
                <tr class="base">
                  <th scope="row">{update.file}</th>
                  <td>{speedLabel(update.current.speed_x)}</td>
                  <td>{update.current.fleurs_en_wer}%</td>
                  <td>{inUseLicense(wire.in_use, update.role)}</td>
                  <td>this is what is running now</td>
                </tr>
                {#each comparison(update) as row (row.variant)}
                  <tr class:pick={row.recommended}>
                    <th scope="row">{row.display_name}</th>
                    <td>{row.speed ?? 'not measured'}</td>
                    <td>{row.error ?? 'not measured'}</td>
                    <td>{row.license ?? 'no licence on the feed'}</td>
                    <td class="rule">{row.verdict}</td>
                  </tr>
                {/each}
              </tbody>
            </table>
          </div>
        </GroupFold>
      {/each}
      {#if shown.length === 0}
        <!-- The role has no entry at all: nothing in the feed joins the model
           it runs, so there is nothing this page could compare. The row's own
           source line says `not in the feed` for that model already. -->
        <p class="note">
          nothing to compare for the {roleWord(shownRole)} role &middot; the model in use has no measured
          rows in the feed
        </p>
      {/if}
    {/if}
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
        {#if wire.rows.length === 0}
          <!-- Enabled, and the feed has not answered: a first enable offline,
               or the boot fetch still out. Its own state, because a box here
               would answer every query with "no entry matches" and the
               discovery chips would point at nothing. -->
          <p class="note">
            the catalogue has not been read yet &middot; the check above is what reads it, and its
            rows land here
          </p>
        {:else if query.trim() === ''}
          <p class="note">
            type a name, or pick a family below &middot; the feed's whole catalogue is already here,
            so this reads nothing off the network
          </p>
          <!-- What can be searched, and what is worth trying: the feed's own
               families and its own fastest rows. Both set the query, so a
               pick is the same mechanism as typing. -->
          <div class="chips">
            {#each familyList as family (family.name)}
              <button class="chip" type="button" onclick={() => (query = family.name)}>
                {family.name}<b>{family.count}</b>
              </button>
            {/each}
          </div>
          {#if fastestRows.length > 0}
            <p class="note">
              fastest on the feed:
              {#each fastestRows as pick, i (pick.variant)}{#if i > 0}{' \u{b7} '}{/if}<button
                  class="link"
                  type="button"
                  onclick={() => (query = pick.variant)}
                  >{pick.variant} {speedLabel(pick.speed?.xrt_wall ?? 0)}</button
                >
              {/each}
            </p>
          {/if}
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
                  href={entryUrl(entry)}
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
      <!-- The material first, then the one button, then the verdict: what a
           run scores on, what it will do, and what it measured. The long
           lists - every saved run, every model a bench can load - live in
           the doors at the bottom, so the section reads as three things
           rather than seven. -->
      <p class="note">{fixturesLine}</p>

      {#if recorder !== null || wire.read_aloud.recording}
        <div class="status" role="status">
          <span class="dot live"></span>
          <span class="t">
            {recorder !== null ? 'recording the passage' : 'the read-aloud set is being recorded'}
          </span>
          {#if recorder !== null}
            <span class="when">{recClock} &middot; {recorder.wire.frames} frames</span>
          {/if}
          <span class="spacer"></span>
          {#if recorder !== null}
            <button class="chip" type="button" onclick={() => onrecordstop(true)}
              >stop and save</button
            >
            <button class="chip" type="button" onclick={() => onrecordstop(false)}>cancel</button>
            <span class="bars meter" aria-hidden="true">
              {#each recCells as cell, at (at)}
                <i class={cell.tone} style={`height:${String(cell.height)}%`}></i>
              {/each}
            </span>
          {:else}
            <span class="detail">Another client is recording it.</span>
          {/if}
          <span class="detail">
            Read the passage below aloud - the recording becomes the read-aloud set, the one corpus
            a bench can score on words that are known.
          </span>
          <span class="detail passage">{wire.read_aloud.passage}</span>
          {#if recordingLine !== null}<span class="detail bad">{recordingLine}</span>{/if}
        </div>
      {:else if wire.read_aloud.recordings.length === 0}
        <!-- The same card the recording draws, so the empty state and the
             running one share an edge: a centred box around a passage puts
             four alignments in one block, and the passage is the thing being
             read. -->
        <div class="status" role="status">
          <span class="dot off"></span>
          <span class="t">the read-aloud set is not recorded yet</span>
          <span class="spacer"></span>
          <button class="chip" type="button" disabled={busy} onclick={onrecord}
            >record the passage</button
          >
          <span class="detail">
            Read the passage below aloud once and save the recording &middot; a bench over the takes
            compares two models' words, and this is the one corpus a bench can score on words that
            are known:
          </span>
          <span class="detail passage">{wire.read_aloud.passage}</span>
          {#if wire.read_aloud.error !== null}
            <span class="detail bad">{wire.read_aloud.error}</span>
          {/if}
          {#if recordingLine !== null}<span class="detail bad">{recordingLine}</span>{/if}
        </div>
      {:else}
        <p class="note">
          the read-aloud set &middot; every recording is scored against the one passage on
          {wire.read_aloud.terms.length} known terms &middot; a run over them reads term accuracy and
          word error, where the takes read agreement
        </p>
        <ul class="list" aria-label="Read-aloud recordings">
          {#each wire.read_aloud.recordings as recording (recording.id)}
            <li>
              <span class="rec">
                <span class="nm">{recordingLength(recording)}</span>
                <span class="facts">{@render facts(recordingFacts(recording))}</span>
              </span>
              <button
                class="chip"
                type="button"
                disabled={busy}
                onclick={() => onrecorddelete(recording)}>delete</button
              >
            </li>
          {/each}
        </ul>
        <button class="chip add" type="button" disabled={busy} onclick={onrecord}
          >record another</button
        >
        {#if wire.read_aloud.error !== null}
          <p class="note bad">{wire.read_aloud.error}</p>
        {/if}
        {#if recordingLine !== null}<p class="note bad">{recordingLine}</p>{/if}
      {/if}

      <!-- One press, one verdict. The card prices the press before it
           spends, says where the sweep is while it runs, and stands the
           verdict afterwards: whether what this machine runs is the best of
           what was scored, and what to switch to when it is not. -->
      {#if sweep !== null}
        <div class="status" role="status">
          <span class="dot live"></span>
          <span class="t">{sweepLine ?? 'drawing up the runs'}</span>
          <span class="when">{tierWord(sweep.tier)} &middot; {sweep.runs.length} runs</span>
          <span class="spacer"></span>
          <button class="chip" type="button" onclick={onsweepcancel}>stop the sweep</button>
          <span class="detail">
            each run loads one model and scores it on the same corpus &middot; what the sweep
            downloaded and nobody kept goes back off the disk when the verdict is in
          </span>
        </div>
      {:else if verdicts.length > 0}
        {#each verdicts as verdict (verdict.role)}
          <div class="status">
            <span class="dot {verdict.onBest ? 'ok' : 'warn'}"></span>
            <span class="t">{sweepHeadline(verdict)}</span>
            <span class="when">{benchRoleWord(verdict.role)}</span>
            <span class="spacer"></span>
            {#if !verdict.onBest}
              <button
                class="chip"
                type="button"
                disabled={busy}
                onclick={() =>
                  onadopt(
                    verdict.best.run.variant,
                    verdict.role === 'cleanup' ? 'normalization' : 'transcribing',
                  )}>switch to it</button
              >
            {/if}
            <span class="detail">{sweepScope(verdict)}</span>
            <span class="detail">{@render facts(resultFacts(verdict.best.result))}</span>
            {#if verdict.baseline !== null && !verdict.onBest}
              <span class="detail">
                what you run, {verdict.baseline.target.file}: {@render facts(
                  resultFacts(verdict.baseline),
                )}
              </span>
            {/if}
            {#if !verdict.onBest}
              <span class="detail">
                {adoptCost(verdict) === 0
                  ? 'already on this machine'
                  : `switching downloads ${sizeLabel(adoptCost(verdict))} again - the sweep took the file back when the verdict came in`}
              </span>
            {/if}
          </div>
        {/each}
        {#if verdicts[0]?.tier === 'consensus'}
          <p class="note">
            scored on your takes, where a run reads as agreement with the words the model in use
            recorded beside each one, not as correctness &middot; record the read-aloud passage
            above and press again to score against known words
          </p>
        {/if}
        <button class="chip add" type="button" disabled={busy} onclick={onsweep}
          >benchmark again</button
        >
      {:else}
        <div class="status">
          <span class="dot off"></span>
          <span class="t">select the best models, and score them here</span>
          <span class="spacer"></span>
          <button
            class="chip"
            type="button"
            disabled={busy || nextSweep.runs.length === 0}
            onclick={onsweep}>run the benchmark</button
          >
          <span class="detail">{sweepShape}</span>
          <span class="detail">{sweepPrice}</span>
        </div>
      {/if}

      {#if bench !== null && (wire.bench.state !== 'failed' || failureShown)}
        {@render op(
          bench,
          wire.bench.state === 'failed' ? () => (dismissedFailure = failureKey) : null,
        )}
      {/if}

      <!-- The doors: what a run measured, and what can be run by hand. Both
           lists are long, and neither is what the section is for, so they
           fold - the door carries the count so a shut one never reads as an
           empty one. -->
      <GroupFold heading="every run scored here" count={wire.results.length}>
        {#if wire.results.length === 0}
          <p class="note">
            nothing has been scored on this machine yet &middot; a press above leaves its runs here
          </p>
        {/if}
        {#each wire.results as result (`${result.target.role}/${result.target.file}/${result.tier}/${result.corpus.sha256}`)}
          <div class="status">
            <span class="dot ok"></span>
            <span class="t">{result.target.file}</span>
            <span class="when">{tierWord(result.tier)}</span>
            <span class="spacer"></span>
            {#if resultWhen(result) !== null}<span class="when">{resultWhen(result)}</span>{/if}
            <button class="chip" type="button" disabled={busy} onclick={() => onbenchdelete(result)}
              >delete</button
            >
            <span class="detail">{@render facts(resultFacts(result))}</span>
            <span class="detail">{resultVerdict(result, wire.in_use, wire.results)}</span>
          </div>
        {/each}
      </GroupFold>

      <GroupFold heading="models a bench can run" count={targets.length}>
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
                  {#if wire.read_aloud.recordings.length > 0}
                    <button
                      class="chip"
                      type="button"
                      disabled={busy}
                      onclick={() => onbench(row.target, 'read_aloud')}>score the read-aloud</button
                    >
                  {/if}
                  {#if !row.current && wire.installed.some((model) => model.file === row.target.file)}
                    <button
                      class="chip"
                      type="button"
                      disabled={busy}
                      title="remove the file and its record from this machine"
                      onclick={() => onuninstall(row.target.file)}>remove</button
                    >
                  {/if}
                {/if}
              </li>
            {/each}
          </ul>
          <p class="note">
            the run scores the candidate against your own takes - its words against the words the
            model in use recorded beside each one - and against the read-aloud passage once you have
            recorded that &middot; the candidate takes its own role's slot and the other role runs
            what you have now, so a cleanup run is this transcribing model plus that normalizer
            &middot; every number measured on this machine
          </p>
        {/if}
      </GroupFold>
    {/if}
  </section>
</main>
