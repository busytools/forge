/**
 * The effective-config document, as `crates/forge-primitives/src/settings.rs`
 * writes it and `crates/forge-workspace/src/settings.rs` builds it.
 *
 * Narrowed once here, where it enters: the page reads these types and never
 * the raw frame. A class or kind this client does not know is kept as
 * `other` with its own word, so a value the server adds draws plainly rather
 * than disappearing.
 */

/** When an edit to a key takes effect. */
export type SettingsClass = 'live' | 'next_session' | 'restart' | 'not_read_yet' | 'other';

/** The shape of a key's value. */
export type SettingsKind = 'string' | 'number' | 'bool' | 'list' | 'table' | 'set' | 'other';

export type SettingsValue =
  | { kind: 'string'; text: string }
  | { kind: 'number'; text: string }
  | { kind: 'bool'; text: string }
  | { kind: 'list'; items: string[] }
  | { kind: 'table'; rows: { key: string; value: string }[] }
  | { kind: 'set'; items: string[] }
  /** The file does not set the key; the page draws the effective built-in. */
  | { kind: 'unset' }
  | { kind: 'other'; text: string };

export interface SettingsKey {
  path: string;
  class: SettingsClass;
  /** The word the server sent, kept for a class this client does not know. */
  classLabel: string;
  value: SettingsValue;
}

export interface SettingsArea {
  id: string;
  name: string;
  note: string;
  keys: SettingsKey[];
}

export interface SettingsDocument {
  version: string;
  source: string;
  totals: { keys: number; areas: number };
  areas: SettingsArea[];
}

const CLASSES: ReadonlySet<string> = new Set(['live', 'next_session', 'restart', 'not_read_yet']);

const KINDS: ReadonlySet<string> = new Set(['string', 'number', 'bool', 'list', 'table', 'set']);

/** The class the page chips, and the word it draws for one it does not know. */
export function settingsClass(raw: string): { class: SettingsClass; label: string } {
  if (CLASSES.has(raw)) return { class: raw as SettingsClass, label: raw };
  return { class: 'other', label: raw };
}

/** The class's own words, for the legend and a row's chip. */
export function classWords(held: SettingsClass, label: string): string {
  switch (held) {
    case 'live':
      return 'live';
    case 'next_session':
      return 'next session';
    case 'restart':
      return 'restart';
    case 'not_read_yet':
      return 'not read yet';
    case 'other':
      return label === '' ? 'unknown class' : label;
  }
}

/**
 * The whole document, or `null` when the frame is not one. A document this
 * client cannot read is drawn as a failed read, not as an empty config.
 */
export function settingsDocumentFrom(value: unknown): SettingsDocument | null {
  const root = narrow(value);
  if (root === null) return null;
  // A frame whose `areas` is not an array of objects is one this client
  // cannot read. Letting it through as an empty list would draw a failed
  // read as a config that declares nothing.
  if (!Array.isArray(root.areas) || !root.areas.every((area) => narrow(area) !== null)) {
    return null;
  }
  const areas = root.areas.map((area, index) => areaFrom(area, index));
  const keys = areas.reduce((total, area) => total + area.keys.length, 0);
  return {
    version: str(root.version) ?? '',
    source: str(root.source) ?? '',
    totals: {
      keys: num(root.totals, 'keys') ?? keys,
      areas: num(root.totals, 'areas') ?? areas.length,
    },
    areas,
  };
}

function areaFrom(value: unknown, index: number): SettingsArea {
  const area = narrow(value) ?? {};
  const id = str(area.id) ?? `area-${index}`;
  return {
    id,
    name: str(area.name) ?? id,
    note: str(area.note) ?? '',
    keys: arrayOf(area.keys).map((key) => keyFrom(key)),
  };
}

function keyFrom(value: unknown): SettingsKey {
  const key = narrow(value) ?? {};
  const rawClass = str(key.class) ?? '';
  const { class: klass, label } = settingsClass(rawClass);
  return {
    path: str(key.path) ?? '(unnamed key)',
    class: klass,
    classLabel: label,
    value: valueFrom(str(key.kind) ?? '', key.value),
  };
}

function valueFrom(kind: string, value: unknown): SettingsValue {
  if (value === null || value === undefined) return { kind: 'unset' };
  if (!KINDS.has(kind)) return { kind: 'other', text: json(value) };
  switch (kind) {
    case 'string':
      return typeof value === 'string'
        ? { kind: 'string', text: value }
        : { kind: 'other', text: json(value) };
    case 'number':
      return typeof value === 'number' && Number.isFinite(value)
        ? { kind: 'number', text: String(value) }
        : { kind: 'other', text: json(value) };
    case 'bool':
      return typeof value === 'boolean'
        ? { kind: 'bool', text: value ? 'true' : 'false' }
        : { kind: 'other', text: json(value) };
    case 'list':
    case 'set': {
      const items = strings(value);
      return items === null
        ? { kind: 'other', text: json(value) }
        : kind === 'list'
          ? { kind: 'list', items }
          : { kind: 'set', items };
    }
    case 'table': {
      const table = narrow(value);
      if (table === null) return { kind: 'other', text: json(value) };
      const rows = Object.entries(table).map(([name, held]) => ({ key: name, value: shown(held) }));
      return { kind: 'table', rows };
    }
    default:
      return { kind: 'other', text: json(value) };
  }
}

/** A scalar as one line: strings as themselves, anything else as JSON. */
function shown(value: unknown): string {
  if (typeof value === 'string') return value;
  if (typeof value === 'number' || typeof value === 'boolean') return String(value);
  return json(value);
}

function json(value: unknown): string {
  try {
    return JSON.stringify(value) ?? String(value);
  } catch {
    return String(value);
  }
}

function narrow(value: unknown): Record<string, unknown> | null {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return null;
  return Object.fromEntries(Object.entries(value));
}

function str(value: unknown): string | null {
  return typeof value === 'string' ? value : null;
}

function num(value: unknown, key: string): number | null {
  const held = narrow(value);
  const found = held?.[key];
  return typeof found === 'number' && Number.isFinite(found) ? found : null;
}

function arrayOf(value: unknown): unknown[] {
  return Array.isArray(value) ? value : [];
}

function strings(value: unknown): string[] | null {
  return Array.isArray(value) && value.every((item) => typeof item === 'string') ? value : null;
}
