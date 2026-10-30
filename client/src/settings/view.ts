/**
 * The settings page's own derivations: which area is drawn, how its rows
 * group, the classes an area holds and the one a group wears. Nothing here
 * reads the socket, so the layout is testable without one.
 */

import type { SettingsArea, SettingsClass, SettingsDocument, SettingsKey } from '../wire/settings';
import { DEFAULT_MARK, DEFAULT_THEME } from '../wire/types';

/** One instance's rows: an account, an org and one of its projects, a
 *  Slack table and workspace, a model, a server table. */
export interface SettingsGroup {
  label: string;
  class: SettingsClass | null;
  rows: SettingsKey[];
}

/**
 * The area the pane draws. `missing` is a selected id the document no longer
 * holds, which draws as its own state rather than as the first area.
 */
export function selectedArea(
  document: SettingsDocument,
  selected: string | null,
): { area: SettingsArea | null; missing: boolean } {
  if (document.areas.length === 0) return { area: null, missing: false };
  if (selected === null) return { area: document.areas[0] ?? null, missing: false };
  const held = document.areas.find((area) => area.id === selected);
  return held === undefined ? { area: null, missing: true } : { area: held, missing: false };
}

/** How hard an area or a group is to apply: the furthest class it holds. */
export function strongest(keys: SettingsKey[]): SettingsClass | null {
  let strongest: SettingsClass | null = null;
  for (const key of keys) {
    if (strongest === null || classRank(key.class) > classRank(strongest)) strongest = key.class;
  }
  return strongest;
}

/**
 * Every distinct class an area holds, in the legend's order. The rail draws
 * all of them: an area holding both a restart key and an unread one says so,
 * rather than reporting only the furthest.
 */
export function classesOf(keys: SettingsKey[]): SettingsClass[] {
  const held = new Set(keys.map((key) => key.class));
  return [...held].sort((one, two) => classRank(one) - classRank(two));
}

export function classRank(held: SettingsClass): number {
  switch (held) {
    case 'live':
      return 0;
    case 'next_session':
      return 1;
    case 'restart':
      return 2;
    case 'other':
      return 3;
    case 'not_read_yet':
      return 4;
  }
}

/**
 * The area's rows, grouped by the instance their path names and ordered by
 * that label. A row's path is the server's own, so a shape this client does
 * not know still lands in a group rather than being dropped.
 */
export function groupsOf(area: SettingsArea): SettingsGroup[] {
  const groups = new Map<string, SettingsKey[]>();
  for (const key of area.keys) {
    // The server sends the instance names, so a name containing dots is
    // one name rather than a path to split.
    const id = key.group.join('\u0000');
    const rows = groups.get(id);
    if (rows === undefined) groups.set(id, [key]);
    else rows.push(key);
  }
  return [...groups.values()]
    .map((rows) => ({
      label: (rows[0]?.group ?? []).join(' / '),
      rows,
      class: strongest(rows),
    }))
    .sort((one, two) => one.label.localeCompare(two.label));
}

/**
 * The built-in a `client` axis draws when the file does not set it, as the
 * cell's whole phrase. The defaults come from the modules that own them, so
 * a built-in that moves moves here too; `font` names no value because the
 * built-in face has no settable name (`FONT_NAMES` holds only `system`).
 */
const CLIENT_DEFAULTS: Readonly<Record<string, string>> = {
  'client.mark': `built-in ${DEFAULT_MARK}`,
  'client.theme': `built-in ${DEFAULT_THEME}`,
  'client.font': 'the bundled face',
};

/**
 * The built-in a `client` axis draws when the file does not set it. The
 * default is display only: it never reaches the axes path, which reads the
 * raw value and gets `null`.
 */
export function builtInFor(areaId: string, path: string): string | null {
  if (areaId !== 'client') return null;
  return CLIENT_DEFAULTS[path] ?? null;
}
