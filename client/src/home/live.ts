/**
 * The home over a live connection: the snapshot it is answered with, the
 * read that follows an update the snapshot cannot carry, and the folds that
 * draw a field a frame carries exactly.
 *
 * **A row's state is the server's**, folded from the core's own `Live`
 * alongside the roster, so it arrives in the row already decided - including
 * the unseen marks. **A frame that carries a drawn field exactly is applied;
 * a frame whose value the drawn row cannot reproduce is answered with a
 * fresh read.** The failure direction is always the read rather than a wrong
 * value on screen: a frame this build cannot read, a seat the drawn home
 * does not hold, or a row in a state another source owns keeps the read it
 * always asked for. The residual reads are named in the issue the folds
 * landed under (#3063 rows 1-2).
 *
 * `accounts_changed` was the first of these, and it is a replace rather than
 * a fold: the frame carries the whole pool field, so the client draws what
 * the server states without asking for an encode - **when the move is usage
 * only.** The loading state, the settled flag, the listener and a probe
 * failure all feed the project rows' `would_bind` and chip, so a change in
 * any of those still asks for the snapshot that refreshes them.
 *
 * **`prompt_lifecycle` is the one fold that counts.** The frame carries one
 * prompt's state, not the row's lifecycle, so the row is drawn from a
 * per-seat count of prompts in flight; the count is re-based by every
 * snapshot answer, which is what keeps a reconnect from adding to it.
 */

import { writable, type Readable, type Writable } from 'svelte/store';

import { subjectKey } from '../protocol';
import { ReadPace } from '../read-pace';
import type { Connection, ConnectionStatus } from '../socket';
import type { Store } from '../stores';
import { coversHome, serviceReport, type ServiceReport } from '../wire/fleet';
import { tasksFrame } from '../wire/board';
import {
  accountsPayload,
  applyBackgroundWork,
  applyBiometric,
  applyFatalError,
  applyFleetCounts,
  applyLifecycle,
  applyServiceStatus,
  applyVaultLock,
  backgroundWorkFrame,
  biometricFrame,
  fatalErrorFrame,
  homeFrom,
  poolFactsChanged,
  promptLifecycleFrame,
  tasksChanged,
  vaultLockFrame,
  type AccountsWire,
  type HomeWire,
} from '../wire/home';
import type { SessionSlot } from '../wire/types';

/**
 * How long an update waits before it is answered with a read.
 *
 * A burst becomes one read rather than one each: a turn settling emits
 * several at once, and a read answers with the fleet as it stands rather
 * than as it was at the first of them.
 *
 * Exported because `home/live.test.ts` drives this beat on fake timers: the
 * tests wait on the beat, so a longer one must lengthen the drive rather than
 * leave the tests asserting before it fires.
 */
export const COALESCE_MS = 50;

/** What the home has to draw from, and why it has nothing when it does not. */
export interface HomeRead {
  wire: HomeWire | null;
  /** The server's own words for turning the subscription down, or `null`. */
  refused: string | null;
  /**
   * Why the snapshot could not be read, in this client's words, or `null`.
   *
   * **A malformed read is its own state, not an empty home**: the store is
   * already `ready` and its bound cleared before the page narrows it, so a
   * healthy-empty drawing would stand forever with nothing saying why. The
   * page draws this instead. Nothing periodic retries it: the next update
   * that covers the home, entering the page, or a reconnect, reads again.
   */
  failed: string | null;
  /**
   * The core's last service report, or `null`.
   *
   * **It is carried rather than read off the snapshot**, because the reports
   * it passes on are moments the snapshot never holds: a refused board edit
   * says here why it did not land, and a page that only re-read on the
   * update would drop the words. The status-page issue the core HOLDS is the
   * snapshot's own `service_status`, which the home's banner draws.
   */
  report: ServiceReport | null;
}

const NOTHING: HomeRead = { wire: null, refused: null, failed: null, report: null };

/**
 * The collections a v9 home must carry, and the shapes the renderer reads
 * out of them. **Absent is not the older-server shape here**: `homeFrom`
 * defaults the fields an older server omits (a row's `misses`, a page's
 * `unseen`), so a missing or wrong collection is a read this client cannot
 * draw as healthy. The element checks are the ones the fleet rows
 * dereference: a project's `sessions` and a seat's `slot`.
 */
function malformedOf(data: unknown): string | null {
  if (data === null || typeof data !== 'object') return 'the snapshot was not an object';
  const held = data as Record<string, unknown>;
  const wrong: string[] = [];
  const problem = (value: unknown): string | null =>
    Array.isArray(value) ? null : value === undefined ? 'was missing' : 'was not a list';
  for (const field of ['agents', 'projects', 'unseen']) {
    const why = problem(held[field]);
    if (why !== null) wrong.push(`${field} ${why}`);
  }
  if (held['fleet'] !== undefined) {
    const why = problem(held['fleet']);
    if (why !== null) wrong.push(`fleet ${why}`);
  }
  const accounts = held['accounts'] as { loading?: unknown } | undefined;
  const accountsWhy = problem(accounts?.loading);
  if (accountsWhy !== null) wrong.push(`accounts.loading ${accountsWhy}`);
  const dictate = held['dictate'] as { snapshot?: { models?: unknown } } | undefined;
  const dictateWhy = problem(dictate?.snapshot?.models);
  if (dictateWhy !== null) wrong.push(`dictate.snapshot.models ${dictateWhy}`);
  if (Array.isArray(held['projects'])) {
    const broken = held['projects'].some((entry) => {
      const project = (entry as { project?: { sessions?: unknown } | null } | null)?.project;
      return project === undefined || project === null || !Array.isArray(project.sessions);
    });
    if (broken) wrong.push('projects[].project.sessions was not a list');
  }
  if (Array.isArray(held['agents'])) {
    const broken = held['agents'].some((entry) => {
      const slot = (entry as { slot?: unknown } | null)?.slot;
      return slot === undefined || slot === null;
    });
    if (broken) wrong.push('agents[].slot was missing');
  }
  return wrong.length === 0 ? null : wrong.join('; ');
}

/**
 * Watch the home.
 *
 * The subscription, the message listener and the status listener are all let
 * go with the last subscriber, so a connection this page has replaced is not
 * left subscribed, listening, and asking the server for a full encode nobody
 * draws.
 */
/**
 * The home's own reread for a connection, where a page is watching it.
 *
 * **A reader's ask goes through the page's guards**, not around them: a
 * return while a read is in flight would otherwise queue a second whole
 * encode behind the first.
 */
const REREADS = new WeakMap<Connection, () => void>();

/** Ask the home for one read through the page's own guards, if it is held. */
export function rereadHome(connection: Connection): void {
  REREADS.get(connection)?.();
}

export function watchHome(connection: Connection): Readable<HomeRead> {
  let held: Store | null = null;
  // A read is a full encode on the server, so asking again while one is in
  // flight queues them behind each other and the page falls further behind
  // the busier the fleet is. One at a time.
  let reading = false;
  /**
   * Whether an update landed while a read was in flight.
   *
   * **The read in hand was encoded before it**, so its answer already
   * disagrees with the fleet: skipping the update with it leaves the page
   * drawing the state it asked about until something else moves - an ask mark
   * outliving the answer that resolved it, which is the wait Ved met leaving
   * needs-you (#1885). So the update is remembered, and asked for again the
   * moment the answer lands.
   */
  let stale = false;
  /** The core's last service report, read off the update that carried it. */
  let report: ServiceReport | null = null;
  /** The tasks each project last announced, so an unchanged set is no read. */
  const lastTasks = new Map<string, string>();
  /**
   * The newest pool a frame carried, kept across publishes.
   *
   * **A reused snapshot is not a newer pool.** A publish off the held
   * snapshot - the empty/failed redraw, or a refusal - states a pool older
   * than the fold, so the fold rides every publish until the snapshot it
   * belongs to says otherwise.
   */
  let folded: AccountsWire | null = null;
  /**
   * Which read request the fold belongs to, or `null` when it is published.
   *
   * **A fold is newer than the read that was in flight when it arrived, and
   * older than the next one.** Counting requests is what tells the two
   * apart: the answer that lands compares its own number, so a fold from an
   * earlier request cannot ride over a later, newer snapshot.
   */
  let foldRead: number | null = null;
  /** Read requests made so far; the initial subscribe is 0. */
  let reads = 0;
  /** The pool the drawn view holds, which a fresh frame is compared against. */
  let drawnPool: AccountsWire | null = null;
  /**
   * The beat every frame-driven read is asked on, floored so a fleet whose
   * news never stops cannot buy a whole encode per frame (#3060).
   */
  const beat = new ReadPace(() => {
    // **A read that started inside the beat is the one this update wanted.**
    // A return's ask lands here too; queueing a second whole encode behind
    // it would let the first answer clear the flag while the second is in
    // flight, and a third could follow.
    if (held === null || reading || held.state().kind === 'refused') return false;
    reads += 1;
    reading = true;
    return connection.refresh('home');
  }, COALESCE_MS);
  /** The last home draw, so a frame's own facts can be applied into it. */
  let drawn: HomeWire | null = null;
  /** A frame applied while the read in hand was being encoded (#3074). */
  let appliedDuringRead = false;
  /**
   * Prompts in flight per seat, for the `prompt_lifecycle` fold.
   *
   * **Re-based by every snapshot answer**, so a reconnect's first answer
   * resets it rather than adding to it, and a seat the server says is running
   * starts at one. A turn that never reports terminal leaves a Running mark
   * only until the next home read, which the residual completion frames and
   * every return still ask for.
   */
  const livePrompts = new Map<string, number>();
  let stopMessages: (() => void) | null = null;
  let stopStatus: (() => void) | null = null;

  /** Ask for one read, coalescing the updates that arrive inside the beat. */
  function schedule(): void {
    beat.schedule();
  }

  /** Two slots are the same seat; the session id is the occupant, not the address. */
  function sameSeat(a: SessionSlot, b: SessionSlot): boolean {
    return a.org === b.org && a.project === b.project && a.label === b.label;
  }

  /**
   * Publish a fold, and drop any read whose answer it outran.
   *
   * **A read already in flight was encoded before this frame and will land
   * after it**: the landing is dropped and the ask goes again rather than
   * overwriting the frame's news (#3074 review).
   */
  function publishFold(next: HomeWire): void {
    if (next === drawn) return;
    drawn = next;
    view.set({ wire: drawn, refused: null, failed: null, report });
    if (reading) appliedDuringRead = true;
  }

  /**
   * Apply one prompt's place in its seat's queue to the drawn row.
   *
   * **The count is per seat and re-based by every snapshot answer.** A row in
   * a state another source owns - an ask, a failure, a heal - is not folded:
   * the read is the failure direction. Answers whether the frame was
   * consumed; `false` leaves the read owed.
   */
  function foldLifecycle(slot: SessionSlot, state: string): boolean {
    if (drawn === null) return false;
    const at = drawn.agents.findIndex((agent) => sameSeat(agent.slot, slot));
    if (at === -1) return false;
    const agent = drawn.agents[at];
    if (agent === undefined) return false;
    // Another source owns this row's mark: an ask, a failure, a heal.
    if (agent.lifecycle !== 'Idle' && agent.lifecycle !== 'Running') return false;
    const key = subjectKey({ session: slot });
    const held = livePrompts.get(key) ?? (agent.lifecycle === 'Running' ? 1 : 0);
    let next: number;
    switch (state) {
      case 'queued':
        next = held + 1;
        break;
      case 'started':
        // A prompt the CLI starts was queued on this seat; a frame that
        // arrives without its queued one still means a turn is running.
        next = Math.max(held, 1);
        break;
      case 'completed':
      case 'cancelled':
      case 'discarded':
      case 'refused':
        next = Math.max(held - 1, 0);
        break;
      default:
        // A state this build has not seen: the read is the failure direction.
        return false;
    }
    livePrompts.set(key, next);
    const applied = applyLifecycle(drawn, slot, next > 0 ? 'Running' : 'Idle');
    if (applied === null) return false;
    publishFold(applied);
    return true;
  }

  /** Apply the CLI's background registry to one seat's drawn row. */
  function foldBackground(slot: SessionSlot, has: boolean): boolean {
    if (drawn === null) return false;
    const applied = applyBackgroundWork(drawn, slot, has);
    if (applied === null) return false;
    publishFold(applied);
    return true;
  }

  /**
   * Publish the held snapshot, with `pool` replacing its accounts when a
   * frame carried one.
   *
   * **One draw for both paths** - a snapshot's own field and a frame's
   * payload - so every state (refused, empty, malformed, failed) words the
   * same way whichever arrived.
   */
  function read(pool: AccountsWire | null = null): void {
    if (held === null) return;
    drawnPool = null;
    const state = held.state();
    if (state.kind === 'refused') {
      // Nothing to apply into: a frame must not paint the refused subject
      // back (#3074 review).
      drawn = null;
      view.set({ wire: null, refused: state.why, failed: null, report });
      return;
    }
    const data = held.snapshot();
    // A store that says `ready` and holds no snapshot is a read that lost its
    // answer, not a read still owed: drawing it as loading would wait forever.
    if (data === null) {
      const failed = state.kind === 'ready' ? 'the snapshot was empty' : null;
      view.set({ wire: null, refused: null, failed, report });
      return;
    }
    const malformed = malformedOf(data);
    if (malformed !== null) {
      view.set({ wire: null, refused: null, failed: malformed, report });
      return;
    }
    // The snapshot is JSON the server wrote from its own type, and `homeFrom`
    // narrows every union member in it straight afterwards. The guard above
    // covers the collections; the catch covers an element shape neither saw.
    try {
      const wire = homeFrom(data as unknown as HomeWire);
      if (pool !== null) wire.accounts = pool;
      drawnPool = wire.accounts;
      drawn = wire;
      // **Every answer re-bases the lifecycle fold's count**: a reconnect's
      // first answer resets it rather than adding to it, and a seat the
      // server says is running starts at one.
      livePrompts.clear();
      for (const agent of wire.agents) {
        livePrompts.set(subjectKey({ session: agent.slot }), agent.lifecycle === 'Running' ? 1 : 0);
      }
      view.set({ wire, refused: null, failed: null, report });
    } catch (why) {
      view.set({
        wire: null,
        refused: null,
        failed: `the snapshot could not be read: ${why instanceof Error ? why.message : String(why)}`,
        report,
      });
    }
  }

  /**
   * One read through this page's guards: nothing while one is in flight, and
   * nothing from a refused subject. A return asks here directly rather than
   * through the beat, and its ask starts the floor so the beat cannot buy a
   * second whole encode right behind it.
   */
  function reread(): void {
    if (held === null || reading || held.state().kind === 'refused') return;
    reads += 1;
    reading = true;
    if (connection.refresh('home')) beat.went();
  }

  function watch(): void {
    // The home's subscription is where the connection declares what belongs to
    // it rather than to a page: it is usually the first subscribe the app
    // makes (/models gets there first on a deep link; a seat page cannot, it
    // waits on the home's own snapshot), and both capabilities below are per
    // CONNECTION - a host or an answerer that only claimed the role on a
    // session page would not hold it while the reader sits on the home.
    //
    // **`answering` here is for the two viewer hand-offs.** Every prompt
    // reaches every subscriber and a permission, question or tool ask parks
    // for the person whoever is attached, so a client that says nothing loses
    // no ask: a client that connects later reads it from the seat's
    // `pending_asks`. The role decides only whether a browser or terminal
    // hand-off may raise, and what makes an unshown seat wear its glyph is the
    // ask itself, read in the client's own `stateOf`.
    held = connection.subscribe('home', { answering: true });
    reading = false;
    // **Registered with the watch, not the store**, so a re-watch after the
    // last reader left registers it again.
    REREADS.set(connection, reread);
    stopMessages = connection.onMessage((message) => {
      if (message.kind === 'error') {
        // **An error is the home's answer only when it names the home or the
        // store says the subscribe was refused.** Every other error - a
        // dispatch, a read of a file - settles something else and leaves the
        // home's read owed, so it must not redraw the held snapshot over a
        // fold or free the next fold to ride that answer.
        const named = message.subject !== undefined && subjectKey(message.subject) === 'home';
        if (named || held?.state().kind === 'refused') {
          reading = false;
          read(folded);
          // **The staleness stays.** A refused subject can take no read at
          // all - both `schedule` and `reread` refuse one - so the update that
          // landed while this read was in flight is not spent here: it is
          // spent by the snapshot that re-establishes the subject, through the
          // snapshot arm below. Clearing it here dropped that update for good
          // (#2559), and the transport refuses the store before this listener
          // ever sees the frame, so a re-ask from this arm could never fire.
        }
        return;
      }
      if (message.kind === 'snapshot') {
        // A refusal this side attributed to another subject says nothing
        // about the home's read.
        if (message.subject !== 'home') return;
        reading = false;
        if (appliedDuringRead) {
          // **The answer was encoded before the frame already drawn**: it
          // must not overwrite that frame's news. The ask goes again, and the
          // next answer is the one newer than the frame.
          appliedDuringRead = false;
          stale = false;
          schedule();
          return;
        }
        // The answer states the pool itself. A fold that arrived while THIS
        // read was in flight is newer than it and stays; a fold from an
        // earlier request is older and goes.
        if (foldRead !== reads) folded = null;
        foldRead = null;
        read(folded);
        // The answer just landed was encoded before whatever arrived while
        // it was in flight, so that update is spent here rather than lost.
        if (stale) {
          stale = false;
          schedule();
        }
        return;
      }
      if (message.kind !== 'update' || !coversHome(message.update)) return;
      // **A refused subject draws its refusal, and the news is not lost**:
      // nothing may paint over the refusal, but the update is spent by the
      // snapshot that re-establishes the subject (#2559, #3074 review).
      if (held?.state().kind === 'refused') {
        stale = true;
        return;
      }
      // **A frame that carries a drawn field exactly is applied, not answered
      // with a read.** Each fold refuses a frame this build cannot read, and
      // the read is then the failure direction.
      const said = serviceReport(message.update);
      if (said !== null) {
        report = said;
        const applied = applyServiceStatus(drawn, said);
        if (applied !== null) {
          publishFold(applied);
          return;
        }
      }
      const fatal = fatalErrorFrame(message.update);
      if (fatal !== null) {
        const applied = applyFatalError(drawn, fatal);
        if (applied !== null) {
          publishFold(applied);
          return;
        }
      }
      const locked = vaultLockFrame(message.update);
      if (locked !== null) {
        const applied = applyVaultLock(drawn, locked);
        if (applied !== null) {
          publishFold(applied);
          return;
        }
      }
      const enrolments = biometricFrame(message.update);
      if (enrolments !== null) {
        const applied = applyBiometric(drawn, enrolments);
        if (applied !== null) {
          publishFold(applied);
          return;
        }
      }
      const life = promptLifecycleFrame(message.update);
      if (life !== null && foldLifecycle(life.slot, life.state)) return;
      const work = backgroundWorkFrame(message.update);
      if (work !== null && foldBackground(work.slot, work.has)) return;
      // **The pool's frame carries the pool.** It is the whole field the read
      // would answer with, so folding it spends no encode; a frame from an
      // older server carries none, and falls through to the read it always
      // asked for.
      const pool = accountsPayload(message.update);
      if (pool !== null) {
        // **Usage-only moves fold; anything a project row or the card is read
        // from keeps the read.** `would_bind` and the row's chip come off the
        // pool's loading state, its probe failures and its selection signal
        // (`at_cap`/`cooling`), so a bail, a heal, a settle or a cap has to
        // come back with a snapshot.
        if (drawnPool !== null && poolFactsChanged(drawnPool, pool)) {
          // The read this asks for is the newer answer; a fold from an
          // earlier request must not ride over it.
          folded = null;
          foldRead = null;
        } else {
          folded = pool;
          if (reading || held?.snapshot() === null) foldRead = reads;
          else read(pool);
          return;
        }
      }
      const tasks = tasksChanged(message.update);
      if (tasks !== null) {
        // An unchanged set is no news: the chase used to re-announce every
        // project every 30 s, and a full home read for that was the loop
        // #2540 removed.
        if (lastTasks.get(tasks.project) === tasks.digest) return;
        lastTasks.set(tasks.project, tasks.digest);
        // **The frame carries the whole set**, so the fleet row's own counts
        // (the ready queue, the waits on the person, the row count) are
        // applied into the draw instead of answered with a read (rule 32;
        // #3063).
        const frame = tasksFrame(message.update);
        const applied =
          frame === null || drawn === null
            ? null
            : applyFleetCounts(drawn, tasks.project, frame.tasks);
        if (applied !== null) {
          drawn = applied;
          view.set({ wire: drawn, refused: null, failed: null, report });
          // **A read already in flight was encoded before this frame and will
          // land after it**: the landing is dropped and the ask goes again
          // rather than overwriting the frame's news (#3074 review).
          if (reading) appliedDuringRead = true;
          return;
        }
      }
      if (reading) {
        stale = true;
        return;
      }
      schedule();
    });
    // A drop takes any read in flight with it, and the reconnect answers with
    // a snapshot of its own - so the pacing must not stay stuck waiting for
    // an answer that died, and the staleness dies with it.
    stopStatus = connection.onStatus((next: ConnectionStatus) => {
      if (next !== 'open') {
        reading = false;
        stale = false;
        // No read is owed across the drop, so the reconnect's snapshot is
        // fresh and replaces the fold; the page keeps drawing the fold until
        // it lands.
        foldRead = null;
        drawn = null;
        livePrompts.clear();
      }
    });
  }

  function unwatch(): void {
    stopMessages?.();
    stopStatus?.();
    stopMessages = null;
    stopStatus = null;
    beat.stop();
    // The subscription goes with the last subscriber too, or a connection
    // nothing draws from stays counted against a seat it is not showing.
    connection.unsubscribe('home');
    held = null;
    livePrompts.clear();
  }

  const view: Writable<HomeRead> = writable(NOTHING, () => {
    watch();
    read();
    return unwatch;
  });

  return { subscribe: view.subscribe };
}
