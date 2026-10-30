/**
 * The settings editor's draft: edited values, added entries and removals,
 * and the one conversion to a save's edits. Pure, so the count, the requests
 * and the restart banner are testable without a page.
 */

import type {
  SettingsDocument,
  SettingsEditKind,
  SettingsEditRequest,
  SettingsKey,
  SettingsValue,
} from '../wire/settings';

export interface EntryField {
  key: string;
  kind: SettingsEditKind;
  required: boolean;
  allowed?: string[];
}

/** The fields a new entry of each repeated group opens with. */
export const ENTRY_FIELDS: Readonly<Record<string, EntryField[]>> = {
  account: [
    { key: 'display_name', kind: 'string', required: true },
    {
      key: 'provider',
      kind: 'enum',
      required: true,
      allowed: ['anthropic', 'codex', 'openrouter', 'zai'],
    },
    { key: 'base_url', kind: 'string', required: false },
    { key: 'token', kind: 'string', required: true },
    { key: 'models', kind: 'list', required: true },
  ],
  project: [
    { key: 'name', kind: 'string', required: true },
    { key: 'path', kind: 'string', required: true },
    { key: 'model', kind: 'string', required: true },
    { key: 'auto_start', kind: 'bool', required: false },
    {
      key: 'permission_mode',
      kind: 'enum',
      required: false,
      allowed: ['default', 'acceptEdits', 'plan', 'dontAsk', 'auto', 'bypassPermissions'],
    },
    { key: 'max_workers', kind: 'number', required: false },
  ],
  slack: [
    { key: 'workspace', kind: 'string', required: true },
    { key: 'token', kind: 'string', required: true },
    { key: 'poll_seconds', kind: 'number', required: false },
    { key: 'dm_poll_seconds', kind: 'number', required: false },
  ],
  model: [{ key: 'auto_compact_window', kind: 'number', required: true }],
};

export interface AddedEntry {
  area: string;
  group: string;
  path: string;
  label: string;
  fields: Record<string, unknown>;
}

export interface Draft {
  /** Key path -> its edited value; `null` clears an optional key. */
  values: Record<string, unknown>;
  adds: Record<string, AddedEntry>;
  removes: Record<string, { area: string; group: string; path: string; label: string }>;
}

export function emptyDraft(): Draft {
  return { values: {}, adds: {}, removes: {} };
}

/** The value a key draws, as the editor's typed shape. */
export function valueOf(value: SettingsValue): unknown {
  switch (value.kind) {
    case 'string':
    case 'number':
      return value.kind === 'number' ? Number(value.text) : value.text;
    case 'bool':
      return value.text === 'true';
    case 'list':
    case 'set':
      return [...value.items];
    case 'unset':
      return null;
    case 'table':
      return Object.fromEntries(value.rows.map((row) => [row.key, row.value]));
    case 'other':
      return value.text;
  }
}

export function findKey(document: SettingsDocument, path: string): SettingsKey | null {
  for (const area of document.areas) {
    const found = area.keys.find((key) => key.path === path);
    if (found !== undefined) return found;
  }
  return null;
}

export function areaOf(document: SettingsDocument, path: string): string | null {
  for (const area of document.areas) {
    if (area.keys.some((key) => key.path === path)) return area.id;
  }
  return null;
}

export function currentValue(draft: Draft, key: SettingsKey): unknown {
  return key.path in draft.values ? draft.values[key.path] : valueOf(key.value);
}

export function currentEntryValue(draft: Draft, path: string, field: string): unknown {
  const added = draft.adds[path];
  return added === undefined ? undefined : added.fields[field];
}

function same(one: unknown, two: unknown): boolean {
  return JSON.stringify(one) === JSON.stringify(two);
}

/** Record an edited value, dropping it when it is the loaded one again. */
export function setValue(draft: Draft, key: SettingsKey, value: unknown): void {
  if (same(value, valueOf(key.value))) delete draft.values[key.path];
  else draft.values[key.path] = value;
}

export function setEntryValue(draft: Draft, path: string, field: string, value: unknown): void {
  const added = draft.adds[path];
  if (added !== undefined) added.fields[field] = value;
}

/** How many changes a save would carry. */
export function dirtyCount(draft: Draft): number {
  return (
    Object.keys(draft.values).length +
    Object.keys(draft.adds).length +
    Object.keys(draft.removes).length
  );
}

export function editRequests(draft: Draft, document: SettingsDocument): SettingsEditRequest[] {
  const requests: SettingsEditRequest[] = [];
  for (const [path, value] of Object.entries(draft.values)) {
    const key = findKey(document, path);
    const area = areaOf(document, path);
    if (key === null || area === null) continue;
    requests.push({
      area,
      keyPath: path,
      pattern: key.pattern,
      op: 'set',
      value: value ?? null,
    });
  }
  for (const added of Object.values(draft.adds)) {
    requests.push({
      area: added.area,
      keyPath: added.path,
      pattern: '',
      op: 'add',
      value: added.fields,
    });
  }
  for (const removed of Object.values(draft.removes)) {
    requests.push({
      area: removed.area,
      keyPath: removed.path,
      pattern: '',
      op: 'remove',
    });
  }
  return requests;
}

/** The restart-class keys a save landed, read off the two documents. */
export function restartKeys(
  before: SettingsDocument,
  after: SettingsDocument,
  requests: SettingsEditRequest[],
): string[] {
  const paths = new Set<string>();
  for (const request of requests) {
    if (request.op === 'set') paths.add(request.keyPath);
    if (request.op === 'add') {
      for (const area of after.areas) {
        for (const key of area.keys) {
          if (key.path.startsWith(`${request.keyPath}.`)) paths.add(key.path);
        }
      }
    }
    if (request.op === 'remove') {
      for (const area of before.areas) {
        for (const key of area.keys) {
          if (key.path.startsWith(`${request.keyPath}.`)) paths.add(key.path);
        }
      }
    }
  }
  const restart: string[] = [];
  for (const path of paths) {
    const key = findKey(after, path) ?? findKey(before, path);
    if (key?.class === 'restart') restart.push(path);
  }
  return restart.sort();
}

/** The entry kinds an area can add, by the area's own add button. */
export function addableGroups(areaId: string): string[] {
  switch (areaId) {
    case 'accounts':
      return ['account'];
    case 'connectors':
      return ['slack'];
    case 'models':
      return ['model'];
    default:
      return [];
  }
}

/** A name not already used under `prefix` in the document or the draft. */
export function uniqueName(
  document: SettingsDocument,
  draft: Draft,
  prefix: string,
  base: string,
): string {
  const taken = new Set<string>();
  for (const area of document.areas) {
    for (const key of area.keys) {
      if (key.path.startsWith(prefix)) taken.add(key.path.slice(prefix.length).split('.')[0] ?? '');
    }
  }
  for (const added of Object.values(draft.adds)) {
    if (added.path.startsWith(prefix))
      taken.add(added.path.slice(prefix.length).split('.')[0] ?? '');
  }
  let name = base;
  for (let index = 2; taken.has(name); index += 1) name = `${base}-${index}`;
  return name;
}

/** Add a skeleton entry, named and empty. */
export function addEntry(
  document: SettingsDocument,
  draft: Draft,
  area: string,
  group: string,
  parent: string | null,
): AddedEntry | null {
  const fields = ENTRY_FIELDS[group];
  if (fields === undefined) return null;
  const base = group === 'slack' ? 'new-workspace' : `new-${group}`;
  const prefix =
    group === 'project'
      ? `orgs.${parent ?? ''}.projects.`
      : group === 'account'
        ? 'accounts.'
        : group === 'slack'
          ? 'slack.'
          : 'models.';
  const name = uniqueName(document, draft, prefix, base);
  const path = group === 'project' ? `orgs.${parent ?? ''}.projects.${name}` : `${prefix}${name}`;
  const added: AddedEntry = {
    area,
    group,
    path,
    label: name,
    fields: Object.fromEntries(
      fields.map((field) => [field.key, field.kind === 'list' ? [] : null]),
    ),
  };
  draft.adds[path] = added;
  return added;
}

export function removeEntry(
  draft: Draft,
  area: string,
  group: string,
  path: string,
  label: string,
): void {
  draft.removes[path] = { area, group, path, label };
  delete draft.adds[path];
}

export function undoRemove(draft: Draft, path: string): void {
  delete draft.removes[path];
}
