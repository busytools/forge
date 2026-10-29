/**
 * The browser's data, as every client reads it: the tab rows a strip draws and
 * the narrowing every wire shape goes through.
 *
 * **What draws the browser is `link.ts`.** The shell paints the server's
 * frames in a native overlay and a page with no shell paints them on its own
 * canvas; both talk to the same server, and the tab list, the tab rows and the
 * refusal sentences are the same on both.
 */

import type { BrowserTabList } from '../protocol';
import type { SessionSlot } from '../wire/types';

/**
 * Whether this page runs inside the client shell: the Tauri marker is the one
 * gate.
 *
 * **The platform gate the browser code shares**: it picks the link the
 * browser surface drives, and the notification path asks it too. It is not
 * the only platform check in the code - the shell's own link also asks
 * whether the phone paints its frames itself. What the surface, the strip and
 * the dock talk to is the link, never this.
 */
export function canHost(): boolean {
  return typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window;
}

/**
 * The reason a rejected command carried, as a sentence.
 *
 * Exported for the strip's own reads: a failed read has the shell's own words
 * in its rejection, and a caller that drew anything else would be inventing a
 * reason.
 */
export function whyText(why: unknown): string {
  if (typeof why === 'string' && why !== '') return why;
  // A socket refusal rejects with an Error carrying the server's sentence;
  // the shell's invoke rejects with the sentence itself.
  if (why instanceof Error && why.message !== '') return why.message;
  return `the client's browser viewer failed: ${String(why)}`;
}

/** The bytes a base64 string carries. */
export function bytesOf(base64: string): Uint8Array {
  const binary = atob(base64);
  const bytes = new Uint8Array(binary.length);
  for (let at = 0; at < binary.length; at += 1) {
    bytes[at] = binary.charCodeAt(at);
  }
  return bytes;
}

/** The profile and tab an attach actually took, or the shell's reason. */
export type OpenAnswer = { profile: string; tab: number } | string;

/**
 * What a lower answered: `true` when the page the person sees is down, or
 * the shell's own sentence for why it is not.
 */
export type HideAnswer = true | string;

/**
 * One tab as the strip draws it.
 *
 * `state` is the server's own word, and `stateText` is how a row spells it -
 * a state this page has never seen is drawn as itself rather than as a
 * neighbour it does not mean.
 */
export interface TabRow {
  number: number;
  title: string;
  url: string;
  /** The owning session as `org/project/label`. */
  owner: string;
  /** The server's own word, kept whole for a state this page has not seen. */
  state: string;
  stateText: string;
}

/** How one tab state reads in a row. **A plain object with an own-property
 *  check**: a wire word like `constructor` must read as itself, not as
 *  something inherited from `Object.prototype`. */
const STATE_TEXT: Record<string, string> = {
  in_use: 'in use',
  idle: 'idle',
  held: 'held',
};

/**
 * The strip's rows, from one tab list.
 *
 * **A row that names no owner is refused, not drawn**: every tab belongs to a
 * session, and a row with none cannot say whose calls drive it - so the
 * strip's read fails with the reason rather than inventing an owner.
 */
export function tabRows(list: BrowserTabList): TabRow[] {
  return list.rows.map((row) => {
    const owner = slotLabel(row.owner);
    if (owner === '') {
      throw new Error(`tab ${row.number} names no owner`);
    }
    const state = row.state;
    return {
      number: row.number,
      // **A tab with no title reads as its address**: `about:blank` is the
      // honest name for a blank page, where an empty cell says nothing.
      title: row.title !== '' ? row.title : row.url !== '' ? row.url : `tab ${row.number}`,
      url: row.url,
      owner,
      state,
      stateText: (Object.hasOwn(STATE_TEXT, state) ? STATE_TEXT[state] : undefined) ?? state,
    };
  });
}

/** One tab list from the wire, narrowed once here; a bad envelope throws. */
export function tabListOf(value: unknown): BrowserTabList {
  if (typeof value !== 'object' || value === null) {
    throw new Error('the server sent no tab list');
  }
  const held = value as Record<string, unknown>;
  if (typeof held['profile'] !== 'string' || typeof held['id'] !== 'number') {
    throw new Error('the tab list names no profile');
  }
  const closing = held['closing_at_ms'];
  if (closing !== null && closing !== undefined && typeof closing !== 'number') {
    throw new Error('the tab list carries no readable close time');
  }
  // **A host's state is read, not guessed.** Coercing a missing or malformed
  // value to false drew "down" over a list that never said so.
  if (typeof held['up'] !== 'boolean' || typeof held['connected'] !== 'boolean') {
    throw new Error('the tab list does not say whether its host is up');
  }
  // **A list this page cannot read is a refusal, not an empty strip.** An
  // unreadable `rows` used to become `[]` and the panel drew "no tabs yet",
  // which claims a read that found none.
  const rows = held['rows'];
  if (!Array.isArray(rows)) {
    throw new Error('the tab list carries no rows');
  }
  return {
    profile: held['profile'],
    id: held['id'],
    up: held['up'],
    connected: held['connected'],
    closing_at_ms: typeof closing === 'number' ? closing : null,
    rows: rows.map((row) => tabRowOf(row)),
  };
}

/** One row narrowed at the wire, or a refusal naming what is unreadable. */
function tabRowOf(value: unknown): BrowserTabList['rows'][number] {
  if (typeof value !== 'object' || value === null) {
    throw new Error('a tab row is not an object');
  }
  const row = value as Record<string, unknown>;
  const number = row['number'];
  const title = row['title'];
  const url = row['url'];
  const state = row['state'];
  const owner = row['owner'];
  if (typeof number !== 'number' || !Number.isInteger(number)) {
    throw new Error('a tab row names no number');
  }
  if (typeof title !== 'string' || typeof url !== 'string') {
    throw new Error(`tab ${number} carries no readable title or address`);
  }
  if (typeof state !== 'string') {
    throw new Error(`tab ${number} names no state`);
  }
  if (typeof owner !== 'object' || owner === null) {
    throw new Error(`tab ${number} names no owner`);
  }
  const slot = owner as Record<string, unknown>;
  if (
    typeof slot['org'] !== 'string' ||
    typeof slot['project'] !== 'string' ||
    typeof slot['label'] !== 'string'
  ) {
    throw new Error(`tab ${number} names no owner`);
  }
  return {
    number,
    title,
    url,
    owner: { org: slot['org'], project: slot['project'], label: slot['label'] },
    state,
  };
}

/** A slot as the strip's own line reads it, or `''` when none came. */
function slotLabel(owner: unknown): string {
  if (typeof owner !== 'object' || owner === null) return '';
  const slot = owner as Record<string, unknown>;
  const parts = [slot.org, slot.project, slot.label];
  return parts.every((part) => typeof part === 'string') ? parts.join('/') : '';
}

/** The seat a browser call names, or `null` for the page's own. */
export type BrowserSeat = SessionSlot | null;
