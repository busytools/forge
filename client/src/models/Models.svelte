<script lang="ts">
  import type { Command } from '../protocol';
  import type { Connection } from '../socket';
  import { onDestroy } from 'svelte';
  import { SvelteSet } from 'svelte/reactivity';
  import type {
    BenchResult,
    BenchTarget,
    BenchTier,
    ModelRole,
    ReadAloudRecording,
  } from '../wire/models';
  import { watchModels, type ModelsRead } from './live';
  import ModelsBody from './ModelsBody.svelte';
  import {
    failureKey,
    sweepPlan,
    sweepVerdicts,
    type SweepPlan,
    type SweepRun,
    type SweepVerdict,
  } from './view';
  import { SetRecorder } from './recorder.svelte';

  /**
   * The models page's route: the subject it watches, what the page draws
   * before the read lands, and the actions it dispatches.
   *
   * The subscription is this page's own and goes with it: only this page
   * draws the catalogue, and a client that has left it must not leave forge
   * encoding a read nobody sees.
   */
  let { connection, mark = null }: { connection: Connection | null; mark?: string | null } =
    $props();

  let read = $state<ModelsRead>({ wire: null, refused: null });
  /**
   * The core's own words for a refused action - an install while one runs, an
   * activation on a role `forge.toml` pins - or `null`. It arrives as an
   * `error` frame on the connection, which is why the page listens beside its
   * own read.
   */
  let refusal = $state<string | null>(null);

  $effect(() => {
    const open = connection;
    if (open === null) return;
    return watchModels(open).subscribe((value) => {
      read = value;
    });
  });

  $effect(() => {
    const open = connection;
    if (open === null) return;
    return open.onMessage((message) => {
      if (message.kind === 'error' && message.what === 'dispatch') refusal = message.why;
    });
  });

  /**
   * Dispatch one action and ask for the re-read straight after.
   *
   * The outcome rides the subscription in every case - a landed download or
   * activation is a `DictateModelsChanged` push - so the refresh is what
   * turns the page over to the core's own `downloading` / `loading` state
   * while the work is out. Without it a click draws nothing until it lands.
   * A dispatch with no open socket throws before anything is sent, and that
   * belongs in the same line as a refusal.
   */
  function act(command: Command): void {
    const open = connection;
    if (open === null) return;
    refusal = null;
    try {
      void open.dispatch(command);
    } catch (why) {
      refusal = why instanceof Error ? why.message : String(why);
    }
    open.refresh('dictate_models');
  }

  function check(): void {
    act('dictate_catalogue_check');
  }

  function install(variant: string): void {
    act({ dictate_install: { variant } });
  }

  function activate(file: string, role: ModelRole): void {
    act({ dictate_activate: { role, file } });
  }

  function deactivate(role: ModelRole): void {
    act({ dictate_deactivate: { role } });
  }

  /** Remove one downloaded model - the file and its record. The core refuses
   * it while a role runs the file, in its own words. */
  function uninstall(file: string): void {
    act({ dictate_uninstall: { file } });
  }

  /**
   * The update flow, as the page knows it: the variant being updated to, and
   * the file once the download has landed.
   *
   * One press means download (when it is not here yet) and then load - the
   * two existing core actions chained, with their own states drawn as they
   * move: the progress line while bytes move, the loading line while the
   * engine builds, and a line saying so when the model is in use.
   */
  let updating = $state<{ variant: string; role: ModelRole; file: string | null } | null>(null);
  let updated = $state<{ file: string; role: ModelRole } | null>(null);

  /** Take a variant into a role: download it when it is not here, then load
   * it. The verdict's own "switch" is this with the role it read best in. */
  function adoptTo(variant: string, role: ModelRole): void {
    const wire = read.wire;
    updated = null;
    refusal = null;
    const record = wire?.installed.find((model) => model.variant === variant);
    if (record !== undefined) {
      // Already on this machine: the adopt is the activation itself.
      updating = { variant, role, file: record.file };
      act({ dictate_activate: { role, file: record.file } });
      return;
    }
    updating = { variant, role, file: null };
    act({ dictate_install: { variant } });
  }

  function updateTo(variant: string): void {
    adoptTo(variant, 'transcribing');
  }

  // The chain moves on the core's own pushes: a download that finished makes
  // the record, and the record is what the activation is about. A press is
  // answered by the wire's own states, never by a timer.
  $effect(() => {
    const chain = updating;
    const wire = read.wire;
    if (chain === null || wire === null) return;
    if (chain.file === null) {
      // Waiting on the download: a failure clears the chain and leaves its
      // own failed line standing.
      if (wire.install.state === 'failed') {
        updating = null;
        return;
      }
      if (wire.install.state !== 'idle') return;
      const record = wire.installed.find((model) => model.variant === chain.variant);
      // No record yet: the push carrying it has not landed.
      if (record === undefined) return;
      if (wire.in_use.some((model) => model.file === record.file)) {
        updated = { file: record.file, role: chain.role };
        updating = null;
        return;
      }
      updating = { variant: chain.variant, role: chain.role, file: record.file };
      act({ dictate_activate: { role: chain.role, file: record.file } });
      return;
    }
    if (wire.activate.state === 'failed') {
      updating = null;
      return;
    }
    if (wire.activate.state !== 'idle') return;
    // Idle, and the role runs the file: the swap landed.
    const current = wire.in_use.find((model) => model.role === chain.role);
    if (current?.file === chain.file) {
      updated = { file: chain.file, role: chain.role };
      updating = null;
    }
  });

  function bench(target: BenchTarget, tier: BenchTier): void {
    act({ dictate_bench: { target, tier } });
  }

  /**
   * The sweep: one press that scores the picks on this machine and leaves a
   * verdict.
   *
   * The plan is drawn from the read before anything moves, so the card can
   * say which candidates, how many bytes and which corpus BEFORE the press
   * spends them. The runs then chain over the core's own one-at-a-time
   * actions - install, then bench - the way the update control chains its
   * own two, and the core stays the thing that serializes.
   */
  let sweep = $state<SweepPlan | null>(null);
  /** The last sweep's verdicts, computed ONCE from the read that closed it.
   * A standing verdict is a record of that sweep: re-deriving it from a
   * later read would let its scope name runs it never saw. */
  let verdicts = $state<SweepVerdict[]>([]);
  let sweepLine = $state<string | null>(null);
  /** Where a sweep ended WITHOUT a verdict - stopped, a failure, or a take
   * landing under it - so the card says why instead of just clearing. */
  let sweepNotice = $state<string | null>(null);
  /** The newest result stamp this read carried at the press, the corpus the
   * sweep's own first run lands on - see the chain below for why the second
   * is not read off the results that were already here - and the failure, if
   * any, that was already standing when the press happened. */
  let sweepStamp = $state<string | null>(null);
  let sweepCorpus = $state<string | null>(null);
  let sweepStaleFailure = $state<string | null>(null);
  /** The installs this sweep has already asked for, so a push that lands
   * mid-flight cannot ask twice. Nothing draws from it; the reactive set is
   * what the sheet's lint takes for a mutable one, and it costs nothing. */
  const sweepAsked = new SvelteSet<string>();

  function sweepRun(): void {
    const wire = read.wire;
    if (wire === null || sweep !== null) return;
    const plan = sweepPlan(wire);
    if (plan.runs.length === 0) return;
    sweepLine = null;
    sweepNotice = null;
    sweepAsked.clear();
    sweepStamp = wire.results[0]?.at ?? null;
    sweepCorpus = null;
    sweepStaleFailure = failureKey(wire.bench);
    sweep = plan;
    // The feeds are read fresh at the press: the sweep installs what the
    // newest read named, not what a cache held.
    act('dictate_catalogue_check');
  }

  function sweepCancel(): void {
    // The sweep's own bench keeps running in the core unless it is stopped
    // with the chain: a cancel that only cleared the card would leave a model
    // loading and scoring for a press nobody is waiting on any more.
    if (read.wire?.bench.state === 'running') act('dictate_bench_stop');
    sweep = null;
    sweepNotice = 'the sweep was stopped; what it measured is on the rows below';
  }

  // One chain over the read's own pushes: install each run that is not here,
  // bench each run without a result on the corpus this sweep compares on,
  // then hand the verdict to the page and take the litter back.
  $effect(() => {
    const plan = sweep;
    const wire = read.wire;
    if (plan === null || wire === null) return;
    const results = (run: SweepRun) =>
      run.file === null
        ? []
        : wire.results.filter(
            (result) => result.target.file === run.file && result.tier === plan.tier,
          );
    // **One corpus is one comparison, and it is the one the sweep's own
    // first run lands on.** Anchoring on a result that was already here
    // would be cheaper but not safe: the takes can have moved since it was
    // measured, so it names a corpus today's runs will never produce - the
    // sweep would bench every run, score none, and bench them again. A
    // result from BEFORE the press on the corpus that run confirms counts,
    // because it is then the same comparison.
    const stamp = sweepStamp === null ? 0 : Date.parse(sweepStamp);
    const fresh = (result: BenchResult) => Date.parse(result.at) > stamp;
    if (sweepCorpus === null) {
      const first = plan.runs.flatMap(results).find(fresh);
      if (first !== undefined) sweepCorpus = first.corpus.sha256;
    }
    const corpus = sweepCorpus;
    const scored = (run: SweepRun) =>
      corpus !== null && results(run).some((result) => result.corpus.sha256 === corpus);

    // **A take landing mid-sweep moves the corpus under the runs, and the
    // core recomputes it per run.** The latched corpus would then match
    // nothing a later run produces, and the chain would bench every run
    // again on every push, forever. A fresh result on another corpus is that
    // move, and it ends the sweep by name.
    if (
      corpus !== null &&
      plan.runs.flatMap(results).some((result) => fresh(result) && result.corpus.sha256 !== corpus)
    ) {
      sweepNotice =
        'a take landed while the sweep ran, so its runs no longer share one corpus - press again to score on what is here now';
      sweep = null;
      return;
    }

    const missing = plan.runs.find((run) => run.file === null);
    if (missing !== undefined) {
      if (wire.install.state === 'failed') {
        sweepNotice = `${wire.install.file}: ${wire.install.reason}`;
        sweep = null;
        return;
      }
      if (wire.install.state !== 'idle') return;
      const record = wire.installed.find((model) => model.variant === missing.variant);
      if (record === undefined) {
        if (!sweepAsked.has(missing.variant)) {
          sweepAsked.add(missing.variant);
          sweepLine = `fetching ${missing.variant}`;
          act({ dictate_install: { variant: missing.variant } });
        }
        return;
      }
      missing.file = record.file;
      return;
    }

    // **Only this sweep's own failure ends it.** A settled failure from
    // before the press is not this chain's to report or to wait on: the core
    // keeps it until the next bench, and the bench below is what clears it.
    if (wire.bench.state === 'failed' && failureKey(wire.bench) !== sweepStaleFailure) {
      sweepNotice = `${wire.bench.target.file}: ${wire.bench.reason}`;
      sweep = null;
      return;
    }
    if (wire.bench.state !== 'idle' && wire.bench.state !== 'failed') return;

    const next = plan.runs.find((run) => run.file !== null && !scored(run));
    if (next !== undefined && next.file !== null) {
      sweepLine = `scoring ${next.file}`;
      act({
        dictate_bench: {
          target: { file: next.file, role: next.role, pinned: false },
          tier: plan.tier,
        },
      });
      return;
    }

    // Every run scored: the verdict stands on the page, and the files the
    // sweep fetched for candidates it did not adopt go back where they came
    // from. An adopted one stays because it is what a role runs.
    verdicts = sweepVerdicts(wire, plan);
    for (const run of plan.runs) {
      if (run.installed || run.file === null) continue;
      if (wire.in_use.some((model) => model.file === run.file)) continue;
      act({ dictate_uninstall: { file: run.file } });
    }
    sweepLine = null;
    sweep = null;
  });

  function benchStop(): void {
    act('dictate_bench_stop');
  }

  /**
   * The read-aloud recording, while it runs: the controller owns the
   * microphone and the frames, the core owns the artifact, and the page
   * learns the outcome from the read rather than from this side.
   */
  let recorder = $state<SetRecorder | null>(null);
  /** A line for the card: this side's own failure - a refused microphone, a
   * keep that never reached the socket. */
  let recordingLine = $state<string | null>(null);
  /** The microphone is opening: a second press waits for this one. */
  let opening = $state(false);
  /** Whether the stop this side just made is still the reason the core says
   * a recording is running - the card must not read that as somebody else's
   * until the read has caught up. */
  let recordStopped = $state(false);

  $effect(() => {
    if (read.wire?.read_aloud.recording === false) recordStopped = false;
  });

  // **Leaving the page releases the microphone.** The socket survives a
  // client-side route change, so a recording left running would hold the
  // input with no page drawing it - and coming back would find a card that
  // says a recording is on with no control to stop it.
  onDestroy(() => {
    recorder?.stop(false);
    recorder = null;
  });

  async function startRecording(): Promise<void> {
    const open = connection;
    // **A second press while the microphone is opening is not a second
    // recording.** `begin` is async, so the guard has to be set before the
    // first await or two rapid presses both pass it.
    if (open === null || recorder !== null || opening) return;
    opening = true;
    refusal = null;
    recordingLine = null;
    try {
      recorder = await SetRecorder.begin({
        connection: open,
        onLine: (line) => (recordingLine = line),
        onEnded: () => (recorder = null),
      });
    } finally {
      opening = false;
    }
    if (recorder !== null) open.refresh('dictate_models');
  }

  function record(): void {
    void startRecording();
  }

  function recordStop(keep: boolean): void {
    const running = recorder;
    recorder = null;
    recordStopped = true;
    running?.stop(keep);
  }

  function recordDelete(recording: ReadAloudRecording): void {
    act({ dictate_read_aloud_delete: { id: recording.id } });
  }

  function benchDelete(result: BenchResult): void {
    act({
      dictate_bench_delete: {
        target: result.target,
        tier: result.tier,
        corpus: result.corpus.sha256,
      },
    });
  }
</script>

{#if read.refused !== null}
  <main class="wrap models">
    <!-- The server turned the subscription down, in its own words. The
         address is not what is wrong, so the door is not what is drawn. -->
    <p class="pending">This forge would not answer for the models: {read.refused}</p>
  </main>
{:else if read.wire === null}
  <main class="wrap models"><p class="pending">Reading the models...</p></main>
{:else}
  <ModelsBody
    wire={read.wire}
    oncheck={check}
    oninstall={install}
    onactivate={activate}
    ondeactivate={deactivate}
    onbench={bench}
    onbenchstop={benchStop}
    onrecord={record}
    onrecordstop={recordStop}
    onrecorddelete={recordDelete}
    {recorder}
    {recordingLine}
    {recordStopped}
    onbenchdelete={benchDelete}
    onupdate={updateTo}
    {updated}
    {refusal}
    {mark}
    {sweep}
    {sweepLine}
    {sweepNotice}
    {verdicts}
    onsweep={sweepRun}
    onsweepcancel={sweepCancel}
    onadopt={adoptTo}
    onuninstall={uninstall}
  />
{/if}
