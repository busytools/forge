/**
 * Notifications for asks a person cannot see.
 *
 * **The trigger differs by platform; the rule does not.** A page with no shell
 * runs this module on its own socket. The desktop and the phone run the same
 * rule inside the shell's Rust bridge (`client/src-tauri/src/bridge.rs`),
 * because a parked WKWebView or a frozen Android WebView stops running this
 * page exactly when a notification matters.
 *
 * Both halves are told the same thing on the wire: a seat's pending kind, its
 * `pending_seq`, and the server run's `boot_id` - the ask's identity. A re-read
 * cannot change it and an answer cannot fake it, so an ask is announced once
 * and an answer that shrinks a batch announces nothing.
 *
 * A decision wait on the task board is the second kind: its identity is the
 * task and the moment it was raised, and it notifies once per raise with no
 * focus exemption, because the app being frontmost does not mean the project
 * holding the wait is being looked at.
 */

import { canHost } from './browser/host';
import { orgOf } from './home/view';
import { subjectKey } from './protocol';
import { goTo } from './routes';
import type { AgentRow, HomeWire, PendingKind, WireTime } from './wire/home';
import type { SessionSlot } from './wire/types';

/** One ask the person should hear about. */
export interface AskNotice {
  slot: SessionSlot;
  kind: PendingKind;
}

/** One decision wait as the notify rule reads it. */
export interface WaitRow {
  /** The seat that raised it. */
  slot: SessionSlot;
  /** The task's id, the wait's identity with `at`. */
  task: string;
  /** The ask's own words, verbatim. */
  words: string;
  /** When the wait began; a re-raise carries a later one. */
  at?: WireTime;
}

/** One decision wait that is news. */
export interface WaitNotice {
  slot: SessionSlot;
  task: string;
  words: string;
  /** The other waits the same read carries for this seat. */
  more: number;
}

/** What the page has announced: an ask identity per seat, and a wait's. */
export interface AskState {
  seen: Map<string, string>;
  /** The waits already announced, keyed by seat and task. */
  waitSeen: Map<string, string>;
  /** Whether a roster read has seeded each rule; they share a read, not a flag. */
  seeded: boolean;
  waitsSeeded: boolean;
}

/** Fresh state for one page's launch. */
export function askState(): AskState {
  return { seen: new Map(), waitSeen: new Map(), seeded: false, waitsSeeded: false };
}

/**
 * The asks a roster read raises.
 *
 * **A first read is the launch's own history**: it seeds the marks and
 * announces nothing, so a reload with three parked asks fires none.
 *
 * **Only announced asks are marked.** An ask that arrives while its seat is
 * on screen is left unmarked, so the read after the person looks away
 * announces it - an ask glanced at is not an ask answered.
 */
export function rising(
  state: AskState,
  boot: string,
  rows: AgentRow[],
  shown: SessionSlot | null,
  focused: boolean,
): AskNotice[] {
  const first = !state.seeded;
  state.seeded = true;
  const notices: AskNotice[] = [];
  const live = new Set<string>();
  const at = shown === null ? null : subjectKey({ session: shown });
  for (const row of rows) {
    if (row.pending === null) continue;
    const key = subjectKey({ session: row.slot });
    live.add(key);
    // **The server run scopes the number**: an ask's sequence restarts with
    // the process, so a mark from a previous run can never match this one's.
    const mark = `${boot}:${row.pending}:${row.pending_seq}`;
    if (state.seen.get(key) === mark) continue;
    if (first) {
      state.seen.set(key, mark);
      continue;
    }
    if (key === at && focused) continue;
    state.seen.set(key, mark);
    notices.push({ slot: row.slot, kind: row.pending });
  }
  // An answered ask leaves no mark, so the next one on that seat is news.
  for (const key of [...state.seen.keys()]) {
    if (!live.has(key)) state.seen.delete(key);
  }
  return notices;
}

/**
 * The decision waits one home read raises.
 *
 * **A wait is news once, and a launch is the launch's own history**: the
 * first read seeds and announces nothing, so a reload with three open waits
 * fires none. Its identity is the task and the moment it was raised, scoped
 * to the server run - a re-read cannot change it, and a wait answered and
 * raised again is a new one.
 *
 * **A raise notifies whatever is on screen.** There is no shown-seat
 * exemption here, because the app being frontmost does not mean the project
 * holding the wait is being looked at.
 *
 * A read's waits are grouped by seat, and a seat with any new one raises a
 * single notice naming the first and counting the rest.
 */
export function waitsRising(state: AskState, boot: string, rows: WaitRow[]): WaitNotice[] {
  const first = !state.waitsSeeded;
  state.waitsSeeded = true;
  const live = new Set<string>();
  const seats = new Map<string, { slot: SessionSlot; waits: WaitRow[] }>();
  for (const row of rows) {
    const seat = subjectKey({ session: row.slot });
    live.add(`${seat}:${row.task}`);
    const held = seats.get(seat);
    if (held === undefined) seats.set(seat, { slot: row.slot, waits: [row] });
    else held.waits.push(row);
  }
  const notices: WaitNotice[] = [];
  for (const { slot, waits } of seats.values()) {
    let announced = false;
    for (const wait of waits) {
      const key = `${subjectKey({ session: slot })}:${wait.task}`;
      const at = wait.at;
      const stamp = at === undefined ? '' : `${at.secs_since_epoch}.${at.nanos_since_epoch}`;
      const mark = `${boot}:${stamp}`;
      if (state.waitSeen.get(key) === mark) continue;
      state.waitSeen.set(key, mark);
      if (first) continue;
      if (announced) continue;
      announced = true;
      notices.push({ slot, task: wait.task, words: wait.words, more: waits.length - 1 });
    }
  }
  // An answered wait leaves no mark, so the same task raised again is news.
  for (const key of [...state.waitSeen.keys()]) {
    if (!live.has(key)) state.waitSeen.delete(key);
  }
  return notices;
}

/** Every decision wait the home read carries, with the seat that raised it. */
export function waitRows(wire: HomeWire): WaitRow[] {
  return wire.fleet.flatMap((row) => {
    const org = orgOf(wire, row.project);
    return row.attention.map((wait) => ({
      slot: { org, project: row.project, label: wait.owner },
      task: wait.task,
      words: wait.words,
      at: wait.at,
    }));
  });
}

/** One ask as the OS shows it. */
export function askWords(notice: AskNotice): { title: string; body: string } {
  const { project, label } = notice.slot;
  const seat = label === 'lead' ? project : `${project} \u{b7} ${label}`;
  const body =
    notice.kind === 'question'
      ? 'a question is waiting'
      : notice.kind === 'permission'
        ? 'a permission prompt is waiting'
        : 'a hand-off is waiting';
  return { title: `forge \u{b7} ${seat}`, body };
}

/** One decision wait as the OS shows it. */
export function waitWords(notice: WaitNotice): { title: string; body: string } {
  const { project, label } = notice.slot;
  const seat = label === 'lead' ? project : `${project} \u{b7} ${label}`;
  const others = notice.more === 0 ? '' : ` (and ${notice.more} more)`;
  return { title: `forge \u{b7} ${seat}`, body: `${notice.words}${others}` };
}

/**
 * Whether this page is the one that raises notifications.
 *
 * **A shell's page is not**: its Rust half sees every frame whether or not the
 * page is running, so it raises them there.
 */
export function notifiesFromThePage(): boolean {
  return !canHost();
}

/** One notification, as the page's own surface carries it. */
interface Held {
  slot: SessionSlot;
  words: { title: string; body: string };
}

/** Raise one ask's notification on a page with no shell. */
export function deliverAsk(notice: AskNotice): void {
  notifyPage({ slot: notice.slot, words: askWords(notice) });
}

/** Raise one decision wait's notification on a page with no shell. */
export function deliverWait(notice: WaitNotice): void {
  notifyPage({ slot: notice.slot, words: waitWords(notice) });
}

/** What waits for a gesture that can carry the permission prompt. */
const waiting: Held[] = [];

/** Whether a gesture listener is up. */
let armed = false;

/**
 * The page's own notification.
 *
 * **The permission is asked from a user gesture, not from an ask.** Firefox
 * and Safari refuse a request made without activation, so the prompt would
 * reject and nothing would ever show; the first gesture after a would-notify
 * ask carries it instead. A denial is respected silently and never retried.
 */
function notifyPage(held: Held): void {
  if (typeof Notification === 'undefined') return;
  if (Notification.permission === 'denied') return;
  if (Notification.permission === 'granted') {
    show(held);
    return;
  }
  waiting.push(held);
  if (waiting.length > 8) waiting.shift();
  arm();
}

/** One notification, whose click opens the seat it was raised for. */
function show(held: Held): void {
  const note = new Notification(held.words.title, {
    body: held.words.body,
    tag: subjectKey({ session: held.slot }),
  });
  note.onclick = () => {
    window.focus();
    goTo({ name: 'session', slot: held.slot });
    note.close();
  };
}

/** Hold the next gesture that arrives long enough to ask. */
function arm(): void {
  if (armed) return;
  armed = true;
  const asked = (): void => {
    window.removeEventListener('pointerdown', asked, true);
    window.removeEventListener('keydown', asked, true);
    armed = false;
    const held = waiting.splice(0);
    if (held.length === 0) return;
    /** Put the asks back, and hold the next gesture that can ask for them. */
    const requeue = (): void => {
      waiting.unshift(...held);
      if (waiting.length > 8) waiting.splice(0, waiting.length - 8);
      arm();
    };
    Notification.requestPermission().then(
      (answer) => {
        // **A denial is final for the page**: browsers never prompt again
        // after a block, so the held asks are dropped and no later gesture
        // asks. Anything that is not an answer (`default`) puts the asks
        // back for the next gesture instead of emptying the queue.
        if (answer === 'denied') return;
        if (answer !== 'granted') {
          requeue();
          return;
        }
        for (const entry of held) show(entry);
      },
      () => {
        // A refused request is no answer; the asks wait for the next gesture.
        requeue();
      },
    );
  };
  window.addEventListener('pointerdown', asked, true);
  window.addEventListener('keydown', asked, true);
}

/**
 * A shell's notification click, which names the seat it was posted for.
 *
 * The window is already brought forward by the shell; the route is the page's
 * own, so it moves here.
 */
export async function watchNotificationClicks(): Promise<() => void> {
  if (!canHost()) return () => {};
  const { listen } = await import('@tauri-apps/api/event');
  return await listen<unknown>('client://notification', (event) => {
    const slot = slotFrom(event.payload);
    if (slot !== null) goTo({ name: 'session', slot });
  });
}

/**
 * The seat a notification tap launched the client on.
 *
 * **The platform reads the launch intent before this page exists**, so a tap
 * that cold-starts the app is not an event a page can listen for: the shell
 * remembers the seat under the id the intent carries, and this is the page
 * asking for it once it can route. `null` is every launch no tap opened, and
 * every page with no shell.
 */
export async function launchSeat(): Promise<SessionSlot | null> {
  if (!canHost()) return null;
  try {
    const { invoke } = await import('@tauri-apps/api/core');
    return slotFrom(await invoke<unknown>('client_tap_seat'));
  } catch (why) {
    // **A seat the shell cannot answer is still a launch**: the page routes as
    // if no tap opened it, rather than holding the launch on the command.
    console.warn('the launch seat could not be read', why);
    return null;
  }
}

/** A seat as the shell's event carries it, or `null` when it is not one. */
function slotFrom(value: unknown): SessionSlot | null {
  if (typeof value !== 'object' || value === null) return null;
  const held = value as Record<string, unknown>;
  if (
    typeof held['org'] !== 'string' ||
    typeof held['project'] !== 'string' ||
    typeof held['label'] !== 'string'
  ) {
    return null;
  }
  return { org: held['org'], project: held['project'], label: held['label'] };
}
