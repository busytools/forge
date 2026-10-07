<script lang="ts">
  import type { Command } from '../protocol';
  import type { Connection } from '../socket';
  import type {
    BenchResult,
    BenchTarget,
    BenchTier,
    ModelRole,
    ReadAloudRecording,
  } from '../wire/models';
  import { watchModels, type ModelsRead } from './live';
  import ModelsBody from './ModelsBody.svelte';
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

  function activate(file: string): void {
    act({ dictate_activate: { role: 'transcribing', file } });
  }

  function deactivate(role: ModelRole): void {
    act({ dictate_deactivate: { role } });
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
  let updating = $state<{ variant: string; file: string | null } | null>(null);
  let updated = $state<string | null>(null);

  function updateTo(variant: string): void {
    const wire = read.wire;
    updated = null;
    refusal = null;
    const record = wire?.installed.find((model) => model.variant === variant);
    if (record !== undefined) {
      // Already on this machine: the update is the activation itself.
      updating = { variant, file: record.file };
      act({ dictate_activate: { role: 'transcribing', file: record.file } });
      return;
    }
    updating = { variant, file: null };
    act({ dictate_install: { variant } });
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
        updated = record.file;
        updating = null;
        return;
      }
      updating = { variant: chain.variant, file: record.file };
      act({ dictate_activate: { role: 'transcribing', file: record.file } });
      return;
    }
    if (wire.activate.state === 'failed') {
      updating = null;
      return;
    }
    if (wire.activate.state !== 'idle') return;
    // Idle, and the role runs the file: the swap landed.
    const current = wire.in_use.find((model) => model.role === 'transcribing');
    if (current?.file === chain.file) {
      updated = chain.file;
      updating = null;
    }
  });

  function bench(target: BenchTarget, tier: BenchTier): void {
    act({ dictate_bench: { target, tier } });
  }

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

  async function startRecording(): Promise<void> {
    const open = connection;
    if (open === null || recorder !== null) return;
    refusal = null;
    recordingLine = null;
    recorder = await SetRecorder.begin({
      connection: open,
      onLine: (line) => (recordingLine = line),
      onEnded: () => (recorder = null),
    });
    if (recorder !== null) open.refresh('dictate_models');
  }

  function record(): void {
    void startRecording();
  }

  function recordStop(keep: boolean): void {
    const running = recorder;
    recorder = null;
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
    onbenchdelete={benchDelete}
    onupdate={updateTo}
    {updated}
    {refusal}
    {mark}
  />
{/if}
