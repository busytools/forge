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
 */

import { canHost } from './browser/host';
import { subjectKey } from './protocol';
import { goTo } from './routes';
import type { AgentRow, PendingKind } from './wire/home';
import type { SessionSlot } from './wire/types';

/** One ask the person should hear about. */
export interface AskNotice {
  slot: SessionSlot;
  kind: PendingKind;
}

/** What the page has announced: one ask identity per seat. */
export interface AskState {
  seen: Map<string, string>;
  /** Whether a roster read has seeded the marks. */
  seeded: boolean;
}

/** Fresh state for one page's launch. */
export function askState(): AskState {
  return { seen: new Map(), seeded: false };
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

/** One ask as the OS shows it. */
export function askWords(notice: AskNotice): { title: string; body: string } {
  const { project, label } = notice.slot;
  const seat = label === 'lead' ? project : `${project} \u{b7} ${label}`;
  const body =
    notice.kind === 'question'
      ? 'a question is waiting'
      : notice.kind === 'permission'
        ? 'a permission is waiting'
        : 'a handoff is waiting';
  return { title: `forge \u{b7} ${seat}`, body };
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

/** Raise one ask's notification on a page with no shell. */
export function deliverAsk(notice: AskNotice): void {
  notifyPage(notice, askWords(notice));
}

/** What waits for a gesture that can carry the permission prompt. */
const waiting: { notice: AskNotice; words: { title: string; body: string } }[] = [];

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
export function notifyPage(notice: AskNotice, words: { title: string; body: string }): void {
  if (typeof Notification === 'undefined') return;
  if (Notification.permission === 'denied') return;
  if (Notification.permission === 'granted') {
    show(notice, words);
    return;
  }
  waiting.push({ notice, words });
  if (waiting.length > 8) waiting.shift();
  arm();
}

/** One notification, whose click opens the seat it was raised for. */
function show(notice: AskNotice, words: { title: string; body: string }): void {
  const note = new Notification(words.title, {
    body: words.body,
    tag: subjectKey({ session: notice.slot }),
  });
  note.onclick = () => {
    window.focus();
    goTo({ name: 'session', slot: notice.slot });
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
        // **Only an answer latches.** A browser that refuses without asking
        // reports `default`, so the asks go back and the next gesture may
        // carry them rather than the queue being silently emptied.
        if (answer !== 'granted') {
          requeue();
          return;
        }
        for (const { notice, words } of held) show(notice, words);
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
