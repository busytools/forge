/**
 * The settings page's own derivations: which area is drawn, how its rows
 * group, and the one class an area or a group wears. Nothing here reads the
 * socket, so the layout is testable without one.
 */

import type { SettingsArea, SettingsClass, SettingsDocument, SettingsKey } from '../wire/settings';

/** One instance's rows: an account, an org and one of its projects, a
 * connector's workspace table. */
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

/** The classes an area holds, counted, in the legend's order. */
export function classCounts(area: SettingsArea): Map<SettingsClass, number> {
  const counts = new Map<SettingsClass, number>();
  for (const key of area.keys) counts.set(key.class, (counts.get(key.class) ?? 0) + 1);
  return counts;
}

/**
 * The area's rows, grouped by the instance their path names and ordered by
 * that label. A row's path is the server's own, so a shape this client does
 * not know still lands in a group rather than being dropped.
 */
export function groupsOf(area: SettingsArea): SettingsGroup[] {
  const groups = new Map<string, SettingsKey[]>();
  for (const key of area.keys) {
    const label = groupLabel(area.id, key.path);
    const rows = groups.get(label);
    if (rows === undefined) groups.set(label, [key]);
    else rows.push(key);
  }
  return [...groups.entries()]
    .map(([label, rows]) => ({ label, rows, class: strongest(rows) }))
    .sort((one, two) => one.label.localeCompare(two.label));
}

function groupLabel(areaId: string, path: string): string {
  const parts = path.split('.');
  switch (areaId) {
    case 'accounts':
      return parts[1] ?? 'accounts';
    case 'projects':
      if (parts[0] === 'orgs' && parts[2] === 'projects') {
        return `${parts[1] ?? 'org'} / ${parts[3] ?? 'project'}`;
      }
      return parts[1] ?? 'orgs';
    case 'connectors':
      return parts[0] === 'slack' ? `slack / ${parts[1] ?? ''}` : (parts[0] ?? 'connectors');
    default:
      return '';
  }
}
