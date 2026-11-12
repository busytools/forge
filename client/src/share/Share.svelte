<script lang="ts">
  /**
   * The share sheet: the files another app shared with forge, a seat to send
   * them to, and the words that go with them.
   *
   * **The upload comes first and the prompt second.** The agent is messaged
   * only after the server has answered the paths it wrote, so a failed
   * upload sends nothing; a retry continues from the server's offsets while
   * it still holds the upload and starts over when it does not, and the
   * sheet says which.
   *
   * The box carries the composer's own dictation: one take, one landing, the
   * same `DictateControl` every box in this client wears.
   */
  import { untrack } from 'svelte';

  import {
    Dictation,
    openCapture,
    takeCounts,
    takePhase,
    takeSilenceLine,
  } from '../composer/dictate.svelte';
  import { axesFor, deviceFor, rememberAxes, rememberDevice } from '../composer/dictation';
  import DictateControl from '../composer/DictateControl.svelte';
  import DictationPanel from '../composer/DictationPanel.svelte';
  import Field from '../composer/Field.svelte';
  import { Landing } from '../composer/landing';
  import TakeCard from '../composer/TakeCard.svelte';
  import { joined, noticeLine } from '../composer/view';
  import { composerFrom } from '../composer/wire';
  import { watchSession, type SessionRead } from '../session/live';
  import { DEFAULT_AXES, type DictateAxes } from '../session/wire';
  import type { Connection } from '../socket';
  import { mintPromptId } from '../wire/ids';
  import type { SessionSlot } from '../wire/types';
  import {
    MAX_SHARE_BYTES,
    composePrompt,
    sizeLabel,
    uploadShare,
    type SharedFile,
    type UploadFn,
  } from './upload';

  let {
    connection,
    seats,
    shared,
    dictate: offered = false,
    preselect = null,
    onclose,
    upload = uploadShare,
  }: {
    /** The open connection: the prompt rides it, and the seat's record does. */
    connection: Connection | null;
    /** Every seat the roster carries, in the order the picker draws them. */
    seats: SessionSlot[];
    /** The files the share handed this app, or the reason there are none. */
    shared: { files: SharedFile[]; error: string | null };
    /** Whether this install offers dictation at all. */
    dictate?: boolean;
    /** The seat the reader was last on, offered as the picker's first value. */
    preselect?: SessionSlot | null;
    onclose: () => void;
    /** The upload, as a test hands its own. */
    upload?: UploadFn;
  } = $props();

  /** A seat's own key, which is what the picker's value names. */
  function seatKey(slot: SessionSlot): string {
    return [slot.org, slot.project, slot.label].join('\u0000');
  }

  /** How a seat is named: its project, then a worker's own label. */
  function seatLabel(slot: SessionSlot | null): string {
    if (slot === null) return 'no seat';
    return slot.label === 'lead' ? slot.project : `${slot.project} \u{b7} ${slot.label}`;
  }

  // **The last seat is the picker's first value**, which is the sheet's own
  // preselection; a roster with no last seat opens on the first one, so the
  // sheet is usable without a pick and the person can change it. Read once,
  // at mount, on purpose: after that the picker is the reader's.
  let pickedKey = $state<string | null>(
    untrack(() => {
      if (preselect !== null && seats.some((slot) => seatKey(slot) === seatKey(preselect))) {
        return seatKey(preselect);
      }
      const firstSeat = seats.length === 0 ? undefined : seats[0];
      return firstSeat === undefined ? null : seatKey(firstSeat);
    }),
  );
  const picked = $derived(seats.find((slot) => seatKey(slot) === pickedKey) ?? null);
  const seatId = $derived(picked === null ? '' : seatKey(picked));

  let text = $state('');
  let phase = $state<'idle' | 'uploading' | 'failed' | 'done'>('idle');
  let failure = $state<string | null>(null);
  /** The upload the last attempt began, so a retry can resume it. */
  let uploadId = $state<number | null>(null);
  /** Whether the attempt continued a held upload or began a new one. */
  let resumed = $state<boolean | null>(null);
  /** The offsets the server already held when the attempt began. */
  let received = $state<number[]>([]);
  let progress = $state<{ file: number; sent: number; total: number } | null>(null);
  /** The paths the server wrote; a failure after this point retries only the prompt. */
  let paths = $state<string[]>([]);
  let sentTo = $state<SessionSlot | null>(null);
  let field = $state<HTMLElement | null>(null);

  const total = $derived(shared.files.reduce((sum, file) => sum + file.size, 0));
  const over = $derived(total > MAX_SHARE_BYTES);

  /** How much of the share has crossed, as the bar draws it. */
  const percent = $derived.by(() => {
    if (progress === null || total === 0) return 0;
    const before = received.slice(0, progress.file).reduce((sum, at) => sum + at, 0);
    return Math.min(100, Math.round(((before + progress.sent) / total) * 100));
  });

  // The picked seat's record, for the take's own notice: a landing arrives as
  // the seat's composer state, and the box reads it from there.
  let read = $state<SessionRead>({ wire: null, refused: null, review: false });
  $effect(() => {
    const open = connection;
    const seat = picked;
    if (open === null || seat === null) return;
    return watchSession(open, seat, false).subscribe(($next) => {
      read = $next;
    });
  });
  const composer = $derived(composerFrom(read.wire?.composer));

  /** What `forge.toml` set, which a take with no pick of its own records under. */
  const defaults = $derived(connection?.settings()?.dictate ?? DEFAULT_AXES);
  let axesEdit = $state<DictateAxes | null>(null);
  let deviceEdit = $state<string | null>(null);
  const axes = $derived(axesEdit ?? axesFor(seatId, defaults));
  const device = $derived(deviceEdit ?? deviceFor(seatId));

  function setAxes(next: DictateAxes): void {
    axesEdit = next;
    if (picked !== null) rememberAxes(seatId, next);
  }

  function setDevice(next: string | null): void {
    deviceEdit = next;
    if (picked !== null) rememberDevice(seatId, next);
  }

  /** The seat a take is addressed to; no take can start while none is picked. */
  const NO_SEAT: SessionSlot = { org: '', project: '', label: '' };
  const landing = new Landing('share');
  let dictateLine = $state<{ tone: string; text: string } | null>(null);

  const dictate = new Dictation({
    // The mic is drawn only on an open connection, so no take can reach this
    // with none: the cast is the wiring's required shape, not a promise.
    connection: () => connection as Connection,
    seat: () => picked ?? NO_SEAT,
    surface: 'share',
    shown: () => composer.take !== null,
    options: () => axes,
    device: () => device,
    capture: openCapture,
    onStart: () => {
      dictateLine = null;
      if (picked !== null) landing.arm(seatId, composer.notice);
    },
    onLine: (line) => {
      dictateLine = line;
      landing.drop();
    },
    onRefused: (line) => {
      dictateLine = line;
    },
  });

  const rowPhase = $derived(picked === null ? null : takePhase(dictate, composer.take, picked));
  const rowWire = $derived(picked === null ? null : takeCounts(dictate, picked));
  const rowSilent = $derived(picked === null ? null : takeSilenceLine(dictate, picked));
  const line = $derived(
    dictateLine ?? rowSilent ?? noticeLine(composer.notice, landing.holds(composer.notice)),
  );

  const canSend = $derived(
    phase === 'idle' &&
      picked !== null &&
      connection !== null &&
      shared.files.length > 0 &&
      !over &&
      dictate.take === null &&
      !dictate.opening,
  );

  // A landed take puts its words at the end of the box, once - the same rule
  // every surface that dictates follows.
  $effect(() => {
    const seat = picked;
    if (seat === null) return;
    const words = landing.place(seatId, composer.notice, composer.take?.generation ?? null);
    if (words === null) return;
    text = joined(text, words);
    landing.placed(composer.notice);
    field?.focus();
  });

  /**
   * Upload, then send the prompt.
   *
   * **A retry after the upload landed re-sends only the prompt.** The paths
   * are the upload's answer, so they are kept as soon as they arrive; a
   * second upload would write the files again under new names.
   */
  async function send(): Promise<void> {
    if (picked === null || connection === null) return;
    if (phase === 'uploading' || shared.files.length === 0 || over) return;
    const seat = picked;
    phase = 'uploading';
    failure = null;
    if (paths.length === 0) {
      resumed = null;
      progress = null;
      try {
        const answer = await upload(seat, shared.files, uploadId, (message) => {
          if (message.kind === 'begin') {
            uploadId = message.upload_id;
            resumed = message.resumed;
            received = message.received;
            return;
          }
          progress = { file: message.file, sent: message.sent, total: message.total };
        });
        paths = answer.paths;
      } catch (why) {
        failure = why instanceof Error ? why.message : String(why);
        phase = 'failed';
        return;
      }
    }
    try {
      await connection.dispatch(
        {
          prompt_under: {
            key: seat,
            text: composePrompt(text, paths),
            attachments: [],
            uuid: mintPromptId(),
            source: 'you',
          },
        },
        seat,
      );
    } catch (why) {
      failure = `the files are uploaded; the message did not send: ${
        why instanceof Error ? why.message : String(why)
      }`;
      phase = 'failed';
      return;
    }
    sentTo = seat;
    phase = 'done';
  }

  function retry(): void {
    phase = 'idle';
    void send();
  }
</script>

<main class="wrap share-page">
  <header class="top">
    <h1>share to a seat</h1>
    <button class="chip" type="button" onclick={onclose}>close</button>
  </header>

  {#if shared.error !== null}
    <p class="line bad">The share could not be read: {shared.error}</p>
  {:else if shared.files.length === 0}
    <p class="line">This share carried no files.</p>
  {:else if seats.length === 0}
    <p class="line">No seat is running to send to.</p>
  {:else}
    <section class="files" aria-label="the shared files">
      {#each shared.files as file, index (file.path)}
        <div class="file" class:current={phase === 'uploading' && progress?.file === index}>
          <span class="name">{file.name}</span>
          <span class="size">{sizeLabel(file.size)}</span>
        </div>
      {/each}
      <p class="total" class:over>
        {sizeLabel(total)} of {sizeLabel(MAX_SHARE_BYTES)}
        {#if over}<span class="over">
            - one share carries at most {sizeLabel(MAX_SHARE_BYTES)}</span
          >{/if}
      </p>
    </section>

    <label class="seat">
      <span class="lab">to</span>
      <select
        value={pickedKey ?? ''}
        disabled={phase !== 'idle' || dictate.take !== null}
        onchange={(event) => {
          pickedKey = event.currentTarget.value;
        }}
      >
        {#each seats as slot (seatKey(slot))}
          <option value={seatKey(slot)}>{seatLabel(slot)}</option>
        {/each}
      </select>
    </label>

    {#if connection === null}
      <p class="line">Waiting for the connection...</p>
    {/if}

    <div class="box">
      {#if dictate.take !== null || dictate.opening}
        <TakeCard
          take={composer.take}
          wire={rowWire}
          phase={rowPhase ?? 'ready'}
          oncancel={() => dictate.act('cancel')}
          spread
        />
      {:else}
        <Field
          editor="share"
          bind:value={text}
          placeholder="say something about the files"
          rows={3}
          aria={{ label: 'message' }}
          field={(el: HTMLElement | null) => {
            field = el;
          }}
        />
        <div class="acts">
          {#if offered && connection !== null}
            <DictateControl {dictate} />
          {/if}
          <button class="chip add" type="button" disabled={!canSend} onclick={() => void send()}>
            send
          </button>
        </div>
      {/if}
      {#if line !== null}
        <p class="line {line.tone}">{line.text}</p>
      {/if}
      {#if dictate.panel && picked !== null && connection !== null}
        <DictationPanel
          {connection}
          {axes}
          {defaults}
          bind={composer.bind}
          mode={composer.mode}
          {device}
          onaxes={setAxes}
          ondevice={setDevice}
          onclose={() => {
            dictate.panel = false;
            field?.focus();
          }}
        />
      {/if}
    </div>

    {#if phase === 'uploading'}
      <p class="line">
        {#if progress === null}
          Uploading {shared.files.length === 1 ? 'the file' : 'the files'}...
        {:else}
          Uploading {shared.files[progress.file]?.name ?? 'the files'} - {percent}%
        {/if}
      </p>
      {#if progress !== null}
        <div
          class="bar"
          role="progressbar"
          aria-valuenow={percent}
          aria-valuemin={0}
          aria-valuemax={100}
        >
          <div class="fill" style:width={`${percent}%`}></div>
        </div>
      {/if}
      {#if resumed !== null}
        <p class="line dim">
          {resumed ? "resuming from the server's offsets" : 'starting over from the first byte'}
        </p>
      {/if}
    {:else if phase === 'failed'}
      <p class="line bad">{failure}</p>
      <button class="chip" type="button" onclick={retry}>retry</button>
    {:else if phase === 'done'}
      <p class="line ok">Sent to {seatLabel(sentTo)}</p>
      {#each paths as path (path)}
        <p class="path">{path}</p>
      {/each}
      <button class="chip" type="button" onclick={onclose}>done</button>
    {/if}
  {/if}
</main>

<style>
  h1 {
    font-size: 18px;
    font-weight: 500;
    margin: 0;
  }

  .top {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 14px;
    margin-bottom: 18px;
  }

  .files {
    border: 1px solid var(--line);
    border-radius: 10px;
    background: var(--s1);
    padding: 10px 14px;
    margin-bottom: 14px;
  }

  .file {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: 14px;
    padding: 5px 0;
    border-bottom: 1px solid var(--line);
    font-size: 14px;
  }

  .file:last-of-type {
    border-bottom: 0;
  }

  .file.current .name {
    color: var(--accent);
  }

  .file .name {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .file .size,
  .total {
    color: var(--dim);
    font-family: var(--mono);
    font-size: 12.5px;
    flex: none;
  }

  .total {
    margin-top: 8px;
  }

  .total.over,
  .over {
    color: var(--bad);
  }

  .seat {
    display: flex;
    align-items: center;
    gap: 10px;
    margin-bottom: 14px;
  }

  .seat .lab {
    color: var(--dim);
    font-family: var(--mono);
    font-size: 12.5px;
    text-transform: uppercase;
    letter-spacing: 0.08em;
  }

  .seat select {
    min-height: 44px;
    min-width: 220px;
    padding: 0 12px;
    border: 1px solid var(--ctl);
    border-radius: 9px;
    background: var(--s2);
    color: var(--text);
    font: inherit;
  }

  .box {
    position: relative;
    border: 1px solid var(--ctl);
    border-radius: 12px;
    background: var(--s1);
    padding: 10px;
  }

  .acts {
    display: flex;
    align-items: center;
    justify-content: flex-end;
    gap: 10px;
    margin-top: 8px;
  }

  .bar {
    height: 4px;
    margin-top: 8px;
    border-radius: 2px;
    background: var(--s3);
    overflow: hidden;
  }

  .bar .fill {
    height: 100%;
    background: var(--accent);
    transition: width 120ms linear;
  }

  .line {
    margin: 10px 0 0;
    font-size: 13.5px;
  }

  .line.bad {
    color: var(--bad);
  }

  .line.ok {
    color: var(--ok);
  }

  .line.dim {
    color: var(--dim);
  }

  .path {
    margin: 4px 0 0;
    color: var(--muted);
    font-family: var(--mono);
    font-size: 12.5px;
    overflow-wrap: anywhere;
  }

  button.chip {
    min-height: 44px;
    padding: 0 16px;
  }

  @media (pointer: coarse) {
    button.chip,
    .seat select {
      min-height: 44px;
    }
  }
</style>
