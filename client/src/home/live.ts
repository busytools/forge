/**
 * The home over a live connection: the whole read it is answered with, the
 * patches applied against it, and the folds that draw a field a frame
 * carries exactly.
 *
 * **A row's state is the server's**, folded from the core's own `Live`
 * alongside the roster, so it arrives in the row already decided - including
 * the unseen marks. **A frame that carries a drawn field exactly is applied;
 * a frame whose value the drawn row cannot reproduce is answered by the
 * patch the server derives from the same write, never by a fresh read.** A
 * frame this build cannot read is reported with its payload rather than
 * dropped (rule 25), and a field that moved with no patch behind it is the
 * server's bug to fix, not a read for the page to buy. The residual folds
 * are named in the issue they landed under (#3063 rows 1-2).
 *
 * `accounts_changed` was the first of these, and it is a replace rather than
 * a fold: the frame carries the whole pool field, so the client draws what
 * the server states without asking for an encode: the frame carries the whole
 * pool field and the patch behind it carries what the server derived from it,
 * so a change in any of the pool's facts lands the same way. The one field a
 * frame cannot settle is the row's own lifecycle, and the patch the server
 * derives for the same write is what states it.
 *
 * **`prompt_lifecycle` is the one fold that counts.** The frame carries one
 * prompt's state, not the row's lifecycle, so the row is drawn from a
 * per-seat count of prompts in flight; the count is re-based by every
 * snapshot answer, which is what keeps a reconnect from adding to it.
 */

import { writable, type Readable, type Writable } from 'svelte/store';

import { subjectKey } from '../protocol';
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
  promptLifecycleFrame,
  tasksChanged,
  vaultLockFrame,
  type HomeWire,
  type WireTime,
} from '../wire/home';
import type { PromptState } from '../wire/lifecycle';
import type { SessionSlot } from '../wire/types';

/** What the home has to draw from, and why it has nothing when it does not. */
export interface HomeRead {
  wire: HomeWire | null;
  /** The server's own words for turning the subscription down, or `null`. */
  refused: string | null;
  /**
   * The stream skipped a version, so one whole read is on its way.
   *
   * The record above is still drawn - a gap never blanks a page - and this is
   * what says the figures may be a beat behind.
   */
  catchingUp?: boolean;
  /**
   * Patches the socket could not place, held for the page to say so.
   *
   * Rule 25: a frame this client cannot read is drawn as itself rather than
   * dropped, and the page's own line is where it is drawn.
   */
  heldPatches?: unknown[];
  /**
   * Why the snapshot could not be read, in this client's words, or `null`.
   *
   * **A malformed read is its own state, not an empty home**: the store is
   * already `ready` and its bound cleared before the page narrows it, so a
   * healthy-empty drawing would stand forever with nothing saying why. The
   * page draws this instead. Nothing periodic retries it: a reconnect reads
   * again, and entering the page does not (a route entry asks no home read,
   * #2955).
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
 * The home's own reread for a connection, where a page is watching it.
 *
 * **A return's ask goes through the page's guards**, not around them: a read
 * asked while one is in flight would queue a second whole encode behind the
 * first.
 */
const REREADS = new WeakMap<Connection, () => void>();

/** Ask the home for one read through the page's own guards, if it is held. */
export function rereadHome(connection: Connection): void {
  REREADS.get(connection)?.();
}

/**
 * Watch the home.
 *
 * The subscription, the message listener and the status listener are all let
 * go with the last subscriber, so a connection this page has replaced is not
 * left subscribed, listening, and asking the server for a full encode nobody
 * draws.
 */
export function watchHome(connection: Connection): Readable<HomeRead> {
  let held: Store | null = null;
  /** Whether a return's read is in flight, so a second one waits for it. */
  let reading = false;
  /** The core's last service report, read off the update that carried it. */
  let report: ServiceReport | null = null;
  /**
   * What the last draw was made from, so the store's own notifications -
   * which fire for a pushed frame as much as for a new record - redraw only
   * when the record or the store's state actually moved.
   */
  let seenSnapshot: unknown = null;
  let seenVersion: number | null = null;
  let seenState: string | null = null;
  let seenHeld = 0;
  /** The last home draw, so a frame's own facts can be applied into it. */
  let drawn: HomeWire | null = null;
  let stopStore: (() => void) | null = null;
  /**
   * The prompts each seat is running or holding queued, by uuid.
   *
   * **The frame names the prompt it is about**, so the count is a set of
   * uuids and not a number: a prompt queued behind a running turn is a second
   * entry, a repeat of one uuid is no second prompt, and a terminal removes
   * exactly the prompt it names. A terminal for a uuid this page never
   * counted folds nothing and keeps the read.
   *
   * **A snapshot answer re-bases the set to what the server vouches for**: a
   * seat it says is idle has nothing in flight, so its set goes; a seat it
   * says is running keeps the uuids this page has seen, because a queued
   * prompt has no other source. A socket drop clears every set, so a
   * reconnect's first answer starts from the snapshot alone. A turn that
   * never reports terminal therefore leaves a Running mark only while the
   * server's own lifecycle still says the seat is working.
   */
  const livePrompts = new Map<string, Set<string>>();
  let stopMessages: (() => void) | null = null;
  let stopStatus: (() => void) | null = null;

  /** Two slots are the same seat; the session id is the occupant, not the address. */
  function sameSeat(a: SessionSlot, b: SessionSlot): boolean {
    return a.org === b.org && a.project === b.project && a.label === b.label;
  }

  /** Publish a fold the frame itself stated. */
  function publishFold(next: HomeWire): void {
    if (next === drawn) return;
    drawn = next;
    publish({ wire: drawn, refused: null, failed: null });
  }

  /**
   * Publish a draw, carrying whether the subject is catching up on a gap.
   *
   * **Every draw goes through here**, so the catching-up line can never be
   * cleared by a fold while the read it names is still owed.
   */
  function publish(next: {
    wire: HomeWire | null;
    refused: string | null;
    failed: string | null;
  }): void {
    view.set({
      ...next,
      report,
      catchingUp: held?.state().kind === 'resyncing',
      heldPatches: held?.unexpected() ?? [],
    });
  }

  /**
   * Apply one prompt's place in its seat's queue to the drawn row.
   *
   * **The identity is the frame's own `uuid`**: `queued` adds it, `started`
   * keeps it, and a terminal removes it. A frame naming a prompt this page
   * never counted, or a row in a state another source owns, folds nothing and
   * keeps the read, because the count cannot be reproduced.
   *
   * **A fold moves a row to Running; it never moves one to Idle.** The set is
   * only the prompts this page has SEEN, so an empty set is no proof that the
   * seat is idle: a page that attached mid-turn drew its Running from a read
   * and has no uuids at all, and folding Idle on its own prompt's terminal
   * would contradict the turn still in flight. Idle is therefore the read's
   * to state, and the terminal that empties the set keeps the read.
   *
   * Answers whether the frame was consumed; `false` leaves the read owed.
   */
  function foldLifecycle(slot: SessionSlot, uuid: string, state: PromptState): boolean {
    if (drawn === null) return false;
    const at = drawn.agents.findIndex((agent) => sameSeat(agent.slot, slot));
    if (at === -1) return false;
    const agent = drawn.agents[at];
    if (agent === undefined) return false;
    // Another source owns this row's mark: an ask, a failure, a heal.
    if (agent.lifecycle !== 'Idle' && agent.lifecycle !== 'Running') return false;
    const key = subjectKey({ session: slot });
    const held = livePrompts.get(key) ?? new Set<string>();
    let touched: WireTime | null = null;
    switch (state.kind) {
      case 'queued':
        // A repeat of one uuid is the same prompt announced again, not a
        // second one.
        held.add(uuid);
        break;
      case 'sent':
        // **`sent` is the terminal**: the prompt left the queue when the CLI
        // took it. A prompt this page never counted cannot be balanced by its
        // terminal, so the read is the failure direction.
        if (!held.delete(uuid)) return false;
        // The old read refreshed the row's "last moved" at the turn's start;
        // the frame carries no timestamp, so this is the client clock's
        // reading of the same moment, and a skewed clock draws a briefly wrong
        // age until the next read states the server's own stamp.
        touched = { secs_since_epoch: Math.floor(Date.now() / 1000), nanos_since_epoch: 0 };
        if (held.size === 0) {
          livePrompts.delete(key);
          return false;
        }
        break;
      case 'not_sent':
        if (!held.delete(uuid)) return false;
        // The set empties: the drawn Running may count a prompt this page
        // never saw, so Idle is the read's to state.
        if (held.size === 0) {
          livePrompts.delete(key);
          return false;
        }
        break;
      default:
        // A state this build has not seen: the read is the failure direction.
        return false;
    }
    livePrompts.set(key, held);
    const applied = applyLifecycle(drawn, slot, 'Running', touched);
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
   * Draw the store's record.
   *
   * **The store is the page's whole source**: the socket applies every patch
   * to it in place, so one draw reads whatever the record is now and every
   * state (refused, empty, malformed, failed) words the same way. There is no
   * ask here and nothing to race: a read is this function, not a round trip.
   */
  function read(): void {
    if (held === null) return;
    const state = held.state();
    if (state.kind === 'refused') {
      // Nothing to apply into: a frame must not paint the refused subject
      // back (#3074 review).
      drawn = null;
      publish({ wire: null, refused: state.why, failed: null });
      return;
    }
    const data = held.snapshot();
    // A store that says `ready` and holds no snapshot is a read that lost its
    // answer, not a read still owed: drawing it as loading would wait forever.
    // **The failed branches clear the drawn record too**, or the next foldable
    // frame would repaint the last good snapshot as a healthy one and erase
    // the failure this state exists to draw (rule 22).
    if (data === null) {
      const failed = state.kind === 'ready' ? 'the snapshot was empty' : null;
      drawn = null;
      livePrompts.clear();
      publish({ wire: null, refused: null, failed });
      return;
    }
    const malformed = malformedOf(data);
    if (malformed !== null) {
      drawn = null;
      livePrompts.clear();
      publish({ wire: null, refused: null, failed: malformed });
      return;
    }
    // The snapshot is JSON the server wrote from its own type, and `homeFrom`
    // narrows every union member in it straight afterwards. The guard above
    // covers the collections; the catch covers an element shape neither saw.
    try {
      const wire = homeFrom(data as unknown as HomeWire);
      drawn = wire;
      // **Every answer re-bases the set to what the server vouches for.** A
      // seat it says is idle has nothing in flight, so its set goes; a seat
      // it says is running keeps the uuids this page has seen, because a
      // second queued prompt has no other source. A seat it no longer lists
      // goes too. A socket drop clears every set, so a reconnect's first
      // answer starts from the snapshot alone.
      const listed = new Set(wire.agents.map((agent) => subjectKey({ session: agent.slot })));
      for (const key of [...livePrompts.keys()]) {
        if (!listed.has(key)) livePrompts.delete(key);
      }
      for (const agent of wire.agents) {
        if (agent.lifecycle === 'Idle') livePrompts.delete(subjectKey({ session: agent.slot }));
      }
      publish({ wire, refused: null, failed: null });
    } catch (why) {
      drawn = null;
      livePrompts.clear();
      publish({
        wire: null,
        refused: null,
        failed: `the snapshot could not be read: ${why instanceof Error ? why.message : String(why)}`,
      });
    }
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
    // **A return's read goes through these guards.** One at a time: a second
    // ask while the first is in flight would queue a whole encode behind one
    // already on its way, and the answer lands through the store subscription
    // below, which is what clears the guard.
    REREADS.set(connection, () => {
      if (reading || held?.state().kind === 'refused') return;
      reading = true;
      if (!connection.refresh('home')) reading = false;
    });
    // **The record is redrawn from the store, not asked for.** Every whole
    // read and every patch lands in the store by the socket's own apply path,
    // so this redraws when the record - or the store's own state, a refusal
    // included - actually moved. A pushed frame leaves the record as it was
    // and is applied by the folds below without a redraw.
    stopStore = held.value.subscribe(() => {
      if (held === null) return;
      // **Any store write answers a return's ask, even an unchanged record.**
      // The write fires whether the read found anything moved or not, so the
      // guard releases here rather than on a version change - a return that
      // found nothing must not latch the guard against every later return.
      reading = false;
      const snapshot = held.snapshot();
      const version = held.version();
      const state = held.state().kind;
      // **A held patch redraws too**: the list is how the page says a frame
      // it could not place was kept rather than dropped (rule 25).
      const heldPatches = held.unexpected().length;
      if (
        snapshot === seenSnapshot &&
        version === seenVersion &&
        state === seenState &&
        heldPatches === seenHeld
      ) {
        return;
      }
      seenSnapshot = snapshot;
      seenVersion = version;
      seenState = state;
      seenHeld = heldPatches;
      read();
    });
    stopMessages = connection.onMessage((message) => {
      if (message.kind !== 'update' || !coversHome(message.update)) return;
      // **A refused subject draws its refusal, and the news is not lost**:
      // nothing may paint over the refusal, while the whole read that
      // re-establishes the subject spends what arrived here (#2559).
      if (held?.state().kind === 'refused') return;
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
      if (life !== null && foldLifecycle(life.slot, life.uuid, life.state)) return;
      const work = backgroundWorkFrame(message.update);
      if (work !== null && foldBackground(work.slot, work.has)) return;
      // **The pool's frame carries the pool**, so it is folded into the draw
      // where one is held; a page with no record yet draws it from the read
      // that is already on its way.
      const pool = accountsPayload(message.update);
      if (pool !== null && drawn !== null) {
        publishFold({ ...drawn, accounts: pool });
        return;
      }
      const tasks = tasksChanged(message.update);
      if (tasks !== null) {
        // **The frame carries the whole set**, so the fleet row's own counts
        // (the ready queue, the waits on the person, the row count) are
        // applied into the draw (rule 32; #3063).
        const frame = tasksFrame(message.update);
        const applied =
          frame === null || drawn === null
            ? null
            : applyFleetCounts(drawn, tasks.project, frame.tasks);
        if (applied !== null) {
          drawn = applied;
          publish({ wire: drawn, refused: null, failed: null });
        }
      }
      // **A frame this page cannot fold is not lost**: it marked the home
      // dirty on the server, and the patch that answers it redraws the store
      // within the coalescing window. There is no read to ask for.
    });
    // A drop leaves the store's record in place - the page keeps drawing what
    // it last read while the reconnect re-subscribes; the answer re-seeds the
    // store and the subscription above redraws it.
    stopStatus = connection.onStatus((next: ConnectionStatus) => {
      if (next !== 'open') {
        drawn = null;
        livePrompts.clear();
      }
    });
  }

  function unwatch(): void {
    stopMessages?.();
    stopStatus?.();
    stopStore?.();
    stopMessages = null;
    stopStatus = null;
    stopStore = null;
    // The subscription goes with the last subscriber too, or a connection
    // nothing draws from stays counted against a seat it is not showing.
    connection.unsubscribe('home');
    REREADS.delete(connection);
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
