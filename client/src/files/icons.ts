/**
 * The Files tree's per-type glyphs, from `material-icon-theme`'s own
 * manifest rather than a hand-drawn set: the package maps extensions, exact
 * filenames and folder names to SVGs and carries the folder tints, and the
 * tree only resolves those names to their files.
 *
 * The manifest is the theme's published icon-theme JSON, so its keys are
 * generated literals that cannot be indexed by a runtime path; the one cast
 * below types it as the shape the theme's spec describes.
 */

import manifestJson from 'material-icon-theme/dist/material-icons.json';

interface IconDefinition {
  iconPath: string;
}

interface IconManifest {
  fileExtensions: Record<string, string>;
  fileNames: Record<string, string>;
  folderNames: Record<string, string>;
  folderNamesExpanded: Record<string, string>;
  iconDefinitions: Record<string, IconDefinition>;
  file: string;
  folder: string;
  folderExpanded: string;
}

const manifest = manifestJson as IconManifest;

/**
 * Every icon file the theme ships, as `?url` assets. `no-inline` keeps a
 * small icon a file rather than a data URI, so the tree requests the glyphs
 * it draws instead of carrying the whole set in the page's script.
 */
const iconUrls = import.meta.glob<string>('/node_modules/material-icon-theme/icons/*.svg', {
  eager: true,
  query: '?url&no-inline',
  import: 'default',
});

/**
 * One icon definition's URL, by the SVG basename its manifest entry names.
 * `fallback` is the theme's own default for the kind, used when a rule
 * names an icon this release does not ship.
 */
function urlFor(name: string | undefined, fallback: string): string | null {
  const path =
    manifest.iconDefinitions[name ?? fallback]?.iconPath ??
    manifest.iconDefinitions[fallback]?.iconPath;
  if (path === undefined) return null;
  const file = path.slice(path.lastIndexOf('/') + 1);
  return iconUrls[`/node_modules/material-icon-theme/icons/${file}`] ?? null;
}

/**
 * The icon for a file, by the theme's own order: an exact filename first,
 * then the longest dotted suffix the manifest knows (`a.ts.map` before
 * `a.map`), then the plain file icon.
 */
export function fileIcon(path: string): string | null {
  const name = path.slice(path.lastIndexOf('/') + 1);
  const exact = manifest.fileNames[name];
  if (exact !== undefined) return urlFor(exact, manifest.file);

  const parts = name.split('.');
  for (let at = 1; at < parts.length; at += 1) {
    const suffix = parts.slice(at).join('.');
    const named = manifest.fileExtensions[suffix];
    if (named !== undefined) return urlFor(named, manifest.file);
  }
  return urlFor(manifest.file, manifest.file);
}

/** The icon for a folder's basename, in its open or shut variant. */
export function folderIcon(name: string, open: boolean): string | null {
  const key = name.toLowerCase();
  const named = open ? manifest.folderNamesExpanded[key] : manifest.folderNames[key];
  const fallback = open ? manifest.folderExpanded : manifest.folder;
  return urlFor(named, fallback);
}
