import type { Unit } from './units';
import { workRowKey } from './units';

/**
 * The conversation's rows: what the virtualised list is handed, one item each.
 *
 * **A turn is normally one item**, and the list draws it whole. That is the
 * design, not an accident: a turn's rows are grouped into blocks whose state
 * (an open call, a folded run) lives in one component, and an item per turn
 * keeps that intact.
 *
 * **A turn taller than any reader will scroll in one pass is taken apart**,
 * because the list virtualises ITEMS and nothing else: one 90,000-pixel turn
 * is one item the list must draw in full whenever any part of it is on screen,
 * and a page holding one is a page whose whole transcript is in the DOM at
 * once. Its units become the items instead, in slices, so the list draws only
 * the slice a reader is looking at and the rest stays data - which is what
 * every other turn already gets.
 *
 * **Nothing is dropped and nothing is capped.** Every unit and every work row
 * of the turn lands in exactly one slice, in its own order: the transcript a
 * reader scrolls through is the same one a whole row would have drawn.
 *
 * **The seams are drawn by the app's stylesheet** (`assets/web.css`, the
 * `.turn.part` rules), so a turn taken apart still reads as one turn.
 */

/** One slice of a turn's fold, as an item of the list. */
export interface Part {
  /** What this item draws, in the turn's own order. */
  units: Unit[];
  /** The turn's first item, whose leading block keeps its own top space. */
  first: boolean;
  /** The turn's last item, whose trailing block keeps its own bottom space. */
  last: boolean;
}

/** One item of the conversation's list. */
export interface ChatRow<T> {
  /** What the list keys the item by: the turn's, or the turn's and its own. */
  key: string;
  turn: T;
  /**
   * The slice this item draws, or `null` for a turn the item draws whole.
   *
   * **Null is the ordinary case and is not the same as an empty slice**: a
   * whole row is the turn's own component folding and drawing itself, where
   * a part is handed the units already folded.
   */
  part: Part | null;
}

/**
 * How many drawn rows a turn may carry before it is taken apart.
 *
 * **A count of rows rather than pixels**, because a unit's height is only
 * known once it is drawn, and the turn that motivated this crossed it at
 * hundreds of rows. Below it a turn is a handful of screens at most - one
 * item the list draws where the reader is looking anyway - so the ordinary
 * path is untouched.
 */
export const SPLIT_AT = 120;

/**
 * How many drawn rows one item of a taken-apart turn carries.
 *
 * The point is to bound what a single item costs to construct and lay out, so
 * this is small enough that a few screens of scroll sit inside one item and
 * large enough that a long turn is not turned into hundreds of items: at 30,
 * the 360-row turn this was measured on is twelve items.
 */
export const PART_ROWS = 30;

/** How many rows a unit draws: a run of work draws one per row it holds. */
export function drawnRows(unit: Unit): number {
  return unit.kind === 'leaves' ? Math.max(1, unit.rows.length) : 1;
}

/** How many rows a stretch of units draws. */
function rowsIn(units: readonly Unit[]): number {
  let total = 0;
  for (const unit of units) total += drawnRows(unit);
  return total;
}

/**
 * A run of work cut into slices of at most `limit` rows, as leaves units.
 *
 * **The first slice keeps the run's own key**, so a turn that crosses the
 * split while it is live does not remount the unit the reader is on - the
 * fold named the run by its first row, and that name stays with the slice
 * holding that row. Every later slice is named by the row it starts at, which
 * does not move while rows land at the end of the run: the fold never reorders
 * a run, so an arriving frame cannot rename a slice the reader is looking at.
 */
function cut(unit: Extract<Unit, { kind: 'leaves' }>, limit: number): Unit[] {
  const out: Unit[] = [];
  for (let at = 0; at < unit.rows.length; at += limit) {
    const slice = unit.rows.slice(at, at + limit);
    const first = slice[0];
    const key = at === 0 || first === undefined ? unit.key : `${unit.key}~${workRowKey(first)}`;
    out.push({ kind: 'leaves', key, rows: slice });
  }
  return out;
}

/**
 * The units of a turn as slices of at most `limit` drawn rows.
 *
 * Slices are cut between whole units wherever they can be, so a part holds
 * whole rows and the turn's own block structure survives inside it; only a run
 * too tall for one slice is cut through, and those slices are leaves units of
 * their own.
 */
function slice(units: readonly Unit[], limit: number): Unit[][] {
  const out: Unit[][] = [];
  let held: Unit[] = [];
  let rows = 0;
  const flush = (): void => {
    if (held.length > 0) out.push(held);
    held = [];
    rows = 0;
  };
  for (const unit of units) {
    const add = rowsIn([unit]);
    if (add > limit && unit.kind === 'leaves') {
      flush();
      for (const piece of cut(unit, limit)) out.push([piece]);
      continue;
    }
    if (rows + add > limit) flush();
    held.push(unit);
    rows += add;
  }
  flush();
  return out;
}

/**
 * Whether `part` draws the row a DOM key names.
 *
 * **The key is what `data-k` carries**, which is the turn's own prefix and the
 * unit's key (`Turn.svelte`), or - for a call - the call row's own mark inside
 * a run (`Call.svelte`). The column needs this to send the list to the item
 * that draws a row a jump asked for: with a turn taken apart, the turn's first
 * item can be tens of thousands of pixels from the row.
 */
export function partDraws(part: Part, turnKey: string, key: string): boolean {
  for (const unit of part.units) {
    if (`${turnKey}:${unit.key}` === key) return true;
    if (unit.kind !== 'leaves') continue;
    for (const row of unit.rows) {
      // A call's own mark is `call-` and the row's name; nothing else in a run
      // draws one. The `#<n>` suffix is a repeated name's ordinal (the list
      // gives a duplicate an ordinal so it still draws), so it is matched too.
      if (row.tag === 'call') {
        const drawn = `call-${row.key}`;
        if (key === drawn || key.startsWith(`${drawn}#`)) return true;
      }
    }
  }
  return false;
}

/**
 * The item of `rows` that draws the call `callId`, or the turn's first item.
 *
 * **Matched by the call's identity, not by its DOM name.** The fold names a
 * call row after the occurrence it arrived in (`c-<id>#<frame>#<block>`), and
 * a name built by spelling that from the id alone would stop matching the
 * moment the fold's naming changes - which is exactly how a dispatch jump
 * silently lands on the turn's head. The tool_use id is the identity a caller
 * holds, so it is what is matched.
 */
export function rowOfCall<T extends { key: string }>(
  rows: readonly ChatRow<T>[],
  turnKey: string,
  callId: string,
): number {
  const exact = rows.findIndex(
    (row) =>
      row.turn.key === turnKey &&
      row.part !== null &&
      row.part.units.some(
        (unit) =>
          unit.kind === 'leaves' &&
          unit.rows.some((held) => held.tag === 'call' && held.leaf.id === callId),
      ),
  );
  return exact >= 0 ? exact : firstRowOf(rows, turnKey);
}

/** The first item of the turn `turnKey` names, or -1 for a turn not held. */
export function firstRowOf<T extends { key: string }>(
  rows: readonly ChatRow<T>[],
  turnKey: string,
): number {
  return rows.findIndex((row) => row.turn.key === turnKey);
}

/**
 * The item of `rows` that draws `key` of the turn `turnKey` names, or the
 * turn's first item.
 *
 * **The fallback is the turn itself, not -1**: a whole turn is one item, and
 * a key that no part draws is a row the fold re-keyed - the turn is still the
 * right place to land, and a jump that gave up there would drop the reader at
 * the foot for a row that is one measurement away.
 */
export function rowOfKey<T extends { key: string }>(
  rows: readonly ChatRow<T>[],
  turnKey: string,
  key: string,
): number {
  const exact = rows.findIndex(
    (row) => row.turn.key === turnKey && row.part !== null && partDraws(row.part, turnKey, key),
  );
  return exact >= 0 ? exact : firstRowOf(rows, turnKey);
}

/**
 * The list's items for a page of turns.
 *
 * `unitsOf` folds a turn and answers its units, or answers `null` for a turn
 * the caller has not folded - which is the common case: a small turn is drawn
 * by its own component, and folding every turn of a page up front would charge
 * the open for turns the reader never scrolls to.
 */
export function chatRows<T extends { key: string }>(
  turns: readonly T[],
  unitsOf: (turn: T) => Unit[] | null,
  splitAt: number = SPLIT_AT,
  partRows: number = PART_ROWS,
): ChatRow<T>[] {
  const out: ChatRow<T>[] = [];
  for (const turn of turns) {
    const units = unitsOf(turn);
    if (units === null || rowsIn(units) <= splitAt) {
      out.push({ key: turn.key, turn, part: null });
      continue;
    }
    const parts = slice(units, partRows);
    for (const [at, sliceUnits] of parts.entries()) {
      const head = sliceUnits[0];
      // **The turn's first item keeps the turn's own key**, so a turn that
      // crosses the split while it is live keeps the row the reader is on:
      // the list moves the turn's head and appends the slices after it rather
      // than replacing everything. Every later slice is named by where it
      // starts, which appends cannot move. (`head` is always there; `slice`
      // never answers an empty part.)
      const key = at === 0 || head === undefined ? turn.key : `${turn.key}#${head.key}`;
      out.push({
        key,
        turn,
        part: { units: sliceUnits, first: at === 0, last: at === parts.length - 1 },
      });
    }
  }
  return out;
}
