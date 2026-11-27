/**
 * Applying one subject's patch to the raw record its store holds.
 *
 * The server writes a patch as the fields that moved: **an absent field means
 * unchanged**, a present `null` clears an optional value, and home's and the
 * board's lists cross as `set`/`removed` entries addressed by id. The patch is
 * applied to the bytes the whole read delivered, before any page narrows them,
 * so every narrowing keeps seeing one shape.
 *
 * **A `null` return means this client cannot read the patch.** The socket
 * holds it as unexpected and asks for one whole read - rule 25: a patch this
 * client cannot place is drawn as itself, never dropped.
 */

import type { Subject } from '../protocol';

export function applyPatchFor(subject: Subject, snapshot: unknown, patch: unknown): unknown {
  // A seat's patch is a whole-field replacement on its record, the same form
  // the small records use.
  if (typeof subject !== 'string') return wholeFields(snapshot, patch);
  switch (subject) {
    case 'home':
      return homePatch(snapshot, patch);
    case 'board':
      return boardPatch(snapshot, patch);
    case 'usage':
    case 'dictate_models':
    case 'extensions':
      return wholeFields(snapshot, patch);
    // Dictation's subscribe answer carries no record - its take frames are its
    // deltas - so a patch for it is a shape neither half should send.
    case 'dictate':
      return null;
    default:
      return null;
  }
}

type Bag = Record<string, unknown>;

function isBag(value: unknown): value is Bag {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

/**
 * Every field the patch carries, written over the record.
 *
 * This is the whole rule for the subjects whose lists have no ids (the seat,
 * usage, the models page, extensions), and for every whole field of home and
 * the board.
 */
function wholeFields(snapshot: unknown, patch: unknown): unknown {
  if (!isBag(snapshot) || !isBag(patch)) return null;
  return { ...snapshot, ...patch };
}

/**
 * One id as a match key.
 *
 * The same id arrives twice - once inside the record the read delivered, once
 * in the patch entry - serialised by the same struct, so its JSON is a stable
 * key for both.
 */
function idKey(id: unknown): string {
  return JSON.stringify(id);
}

/** A list field's id, read from the record itself. */
type IdOf = (record: Bag) => unknown;

/**
 * Where one entry names its id and its new value.
 *
 * Home's entries are `{op, id, record}`; the board's project entries are
 * `{op, identity, ...board}`, because a project's identity is a field of the
 * record it belongs to rather than a key beside it.
 */
interface EntryShape {
  id: (entry: Bag) => unknown;
  value: (entry: Bag) => unknown;
}

/**
 * One list field, with its `set`/`removed` entries applied.
 *
 * A set replaces the entry holding that id where it stands and appends one the
 * list does not hold; a removal takes its entry out. Order is the patch's
 * business only for a new entry: the views sort what they draw (the board by
 * rank and status, the home by the roster's own groups), so an appended id
 * lands where its record says it belongs.
 *
 * `build` receives the entry's value and the record it replaces, `null` for
 * one the list does not hold.
 *
 * `null` when either side is not the shape this expects.
 */
function applyEntries(
  list: unknown,
  entries: unknown,
  idOf: IdOf,
  shape: EntryShape,
  build: (value: unknown, existing: Bag | null) => unknown,
): unknown[] | null {
  if (!Array.isArray(list) || !Array.isArray(entries)) return null;
  // `Array.isArray` narrows the value to `any[]`; the list crossing here is
  // JSON, so it is re-typed once at the boundary rather than leaking `any`.
  const next: unknown[] = [...(list as unknown[])];
  for (const raw of entries) {
    if (!isBag(raw)) return null;
    const op = raw['op'];
    const key = idKey(shape.id(raw));
    const at = next.findIndex((held) => isBag(held) && idKey(idOf(held)) === key);
    if (op === 'set') {
      const built = build(shape.value(raw), at >= 0 ? (next[at] as Bag) : null);
      if (built === null) return null;
      if (at >= 0) next[at] = built;
      else next.push(built);
      continue;
    }
    if (op === 'removed') {
      if (at >= 0) next.splice(at, 1);
      continue;
    }
    return null;
  }
  return next;
}

/** The entry's value is the record itself. */
const RECORD_ENTRY: EntryShape = {
  id: (entry) => entry['id'],
  value: (entry) => entry['record'],
};

function asRecord(value: unknown, _existing: Bag | null): unknown {
  return isBag(value) ? value : null;
}

/**
 * The home: its three lists travel by id, every other field is replaced whole.
 *
 * Ids are the ones the records themselves carry - a project by its org and
 * name, a seat by its slot, a fleet row by its project name - so a patch never
 * names an id the record does not also state.
 */
function homePatch(snapshot: unknown, patch: unknown): unknown {
  if (!isBag(snapshot) || !isBag(patch)) return null;
  const next: Bag = { ...snapshot };
  for (const [key, value] of Object.entries(patch)) {
    switch (key) {
      case 'projects': {
        const projects = applyEntries(
          next['projects'],
          value,
          (row) => row['project'],
          RECORD_ENTRY,
          asRecord,
        );
        if (projects === null) return null;
        next['projects'] = projects;
        break;
      }
      case 'agents': {
        const agents = applyEntries(
          next['agents'],
          value,
          (row) => row['slot'],
          RECORD_ENTRY,
          asRecord,
        );
        if (agents === null) return null;
        next['agents'] = agents;
        break;
      }
      case 'fleet': {
        const fleet = applyEntries(
          next['fleet'],
          value,
          (row) => row['project'],
          RECORD_ENTRY,
          asRecord,
        );
        if (fleet === null) return null;
        next['fleet'] = fleet;
        break;
      }
      default:
        next[key] = value;
    }
  }
  return next;
}

/**
 * The board: each project's entry carries only its moved lists, and its rows
 * are addressed by task id inside it.
 */
function boardPatch(snapshot: unknown, patch: unknown): unknown {
  if (!isBag(snapshot) || !isBag(patch)) return null;
  const next: Bag = { ...snapshot };
  const entries = patch['projects'];
  if (entries === undefined) return next;
  const projects = applyEntries(
    next['projects'],
    entries,
    (row) => row['project'],
    { id: (entry) => entry['identity'], value: (entry) => entry },
    buildProject,
  );
  if (projects === null) return null;
  next['projects'] = projects;
  return next;
}

/**
 * One project's entry written over its record, or a new project's whole board.
 *
 * A project the client has never read arrives with every field set, so the
 * identity plus the entry's `board` is the whole record.
 */
function buildProject(entry: unknown, existing: Bag | null): unknown {
  if (!isBag(entry) || !isBag(entry['board'])) return null;
  const base: Bag = { ...(existing ?? { project: entry['identity'], rows: [] }) };
  for (const [key, value] of Object.entries(entry['board'])) {
    if (key === 'rows') {
      const rows = applyEntries(
        base['rows'],
        value,
        (row) => {
          const task = row['task'];
          return isBag(task) ? task['id'] : undefined;
        },
        RECORD_ENTRY,
        asRecord,
      );
      if (rows === null) return null;
      base['rows'] = rows;
      continue;
    }
    base[key] = value;
  }
  return base;
}
