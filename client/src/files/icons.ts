/**
 * The Files tree's per-type glyphs, from `material-icon-theme`'s own
 * manifest rather than a hand-drawn set: the package maps extensions, exact
 * filenames and folder names to SVGs and carries the folder tints, and the
 * tree only resolves those names to their files.
 *
 * The manifest is the theme's published icon-theme JSON. Its keys are mixed
 * case and some carry a parent path (`parent/name`), and VS Code matches
 * both case-insensitively, so the maps below are lowercased once and looked
 * up the same way. Its generated literal keys cannot be indexed by a
 * runtime path; the one cast types it as the shape the theme's spec says.
 */

import { SvelteMap } from 'svelte/reactivity';

import manifestJson from 'material-icon-theme/dist/material-icons.json';

interface IconDefinition {
  iconPath: string;
}

interface IconManifest {
  fileExtensions: Record<string, string>;
  fileNames: Record<string, string>;
  folderNames: Record<string, string>;
  folderNamesExpanded: Record<string, string>;
  rootFolderNamesExpanded: Record<string, string>;
  iconDefinitions: Record<string, IconDefinition>;
  file: string;
  folder: string;
  folderExpanded: string;
  rootFolder: string;
  rootFolderExpanded: string;
}

const manifest = manifestJson as IconManifest;

/** A manifest map keyed the way the theme matches it: lowercased. */
function lowered(record: Record<string, string>): Map<string, string> {
  const out = new Map<string, string>();
  for (const [key, value] of Object.entries(record)) out.set(key.toLowerCase(), value);
  return out;
}

const fileNames = lowered(manifest.fileNames);
const fileExtensions = lowered(manifest.fileExtensions);
const folderNames = lowered(manifest.folderNames);
const folderNamesExpanded = lowered(manifest.folderNamesExpanded);
const rootFolderNamesExpanded = lowered(manifest.rootFolderNamesExpanded);

/**
 * Every icon file the theme ships, as `?url` assets. `no-inline` keeps a
 * small icon a file rather than a data URI.
 */
const iconAssets = new Map<string, string>();
for (const [path, url] of Object.entries(
  import.meta.glob<string>('../../node_modules/material-icon-theme/icons/*.svg', {
    eager: true,
    query: '?url&no-inline',
    import: 'default',
  }),
)) {
  iconAssets.set(path.slice(path.lastIndexOf('/') + 1), url);
}

/**
 * The glyphs fetched so far, as object URLs, and the ones already asked
 * for. A row mounts an `<img>` per glyph, and a filtered tree remounts
 * every row on a keystroke; without this each wave re-read the same SVGs
 * down the custom protocol, which carries no cache headers.
 */
const fetched = new SvelteMap<string, string>();
const asked = new Set<string>();

/** The SVG file a definition names, whether or not this release ships it. */
function iconFile(name: string | undefined, fallback: string): string | null {
  const path =
    manifest.iconDefinitions[name ?? fallback]?.iconPath ??
    manifest.iconDefinitions[fallback]?.iconPath;
  if (path === undefined) return null;
  return path.slice(path.lastIndexOf('/') + 1);
}

/**
 * An icon's src: its cached object URL once fetched, the bundled asset
 * before that. The fetch is started once per icon; a failed one leaves the
 * asset URL drawing.
 */
function src(name: string | undefined, fallback: string): string | null {
  const file = iconFile(name, fallback);
  if (file === null) return null;
  const held = fetched.get(file);
  if (held !== undefined) return held;
  const asset = iconAssets.get(file);
  if (asset === undefined) return null;
  if (!asked.has(file)) {
    asked.add(file);
    void fetch(asset)
      .then((answer) => {
        if (!answer.ok) throw new Error(`the icon answered ${answer.status}`);
        return answer.blob();
      })
      .then((blob) => fetched.set(file, URL.createObjectURL(blob)))
      .catch(() => {
        // The asset URL keeps the glyph on screen; nothing else to do.
      });
  }
  return asset;
}

/** The lowercased basename of a tree path. */
function basename(path: string): string {
  return path.slice(path.lastIndexOf('/') + 1).toLowerCase();
}

/** The theme's `parent/name` key for a path, lowercased, or null at a root. */
function qualified(path: string): string | null {
  const parts = path.toLowerCase().split('/');
  if (parts.length < 2) return null;
  const name = parts.pop() ?? '';
  const parent = parts.pop() ?? '';
  return parent === '' ? null : `${parent}/${name}`;
}

/** A manifest lookup by parent-qualified key first, then the bare name. */
function named(map: ReadonlyMap<string, string>, path: string): string | undefined {
  const key = qualified(path);
  const qualifiedName = key === null ? undefined : map.get(key);
  return qualifiedName ?? map.get(basename(path));
}

/**
 * The icon for a file, in the theme's own order: a parent-qualified or
 * exact filename, then the longest dotted suffix the manifest knows
 * (`foo.d.ts` before `foo.ts`), then the plain file icon.
 */
export function fileIcon(path: string): string | null {
  const exact = named(fileNames, path);
  if (exact !== undefined) return src(exact, manifest.file);

  const parts = basename(path).split('.');
  for (let at = 1; at < parts.length; at += 1) {
    const suffix = parts.slice(at).join('.');
    const extension = fileExtensions.get(suffix);
    if (extension !== undefined) return src(extension, manifest.file);
  }
  return src(manifest.file, manifest.file);
}

/**
 * VS Code's folder rule: an expanded name wins, then the shut name (an open
 * folder with no expanded entry keeps its named shut glyph), then the
 * kind's own fallback.
 */
export function folderIconName(
  shut: string | undefined,
  expanded: string | undefined,
  fallbackShut: string,
  fallbackExpanded: string,
  open: boolean,
): string {
  if (open) return expanded ?? shut ?? fallbackExpanded;
  return shut ?? fallbackShut;
}

/** The icon for a folder's path, in its open or shut variant. */
export function folderIcon(path: string, open: boolean): string | null {
  const name = folderIconName(
    named(folderNames, path),
    named(folderNamesExpanded, path),
    manifest.folder,
    manifest.folderExpanded,
    open,
  );
  return src(name, manifest.folder);
}

/** The icon for the tree's own root row, keyed off the root's basename. */
export function rootIcon(path: string): string | null {
  const name = rootFolderNamesExpanded.get(basename(path)) ?? manifest.rootFolderExpanded;
  return src(name, manifest.rootFolder);
}
