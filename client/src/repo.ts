import { existsSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

/**
 * A repository file's path, from its path at the root.
 *
 * The tests that pin the drawings under `docs/book/` and the server sources
 * and fixtures they mirror read outside the client tree, and so outside the
 * copy Stryker's sandbox is made of: a fixed `../../` depth resolves inside
 * `.stryker-tmp` there, and every one of those reads dies ENOENT before a
 * mutant runs. The root is found by walking up to the checkout instead, which
 * is the same one from either, and reads the file the test is about rather
 * than a copy that drifts.
 */
export function repoPath(relative: string): string {
  let dir = path.dirname(fileURLToPath(import.meta.url));
  while (!existsSync(path.join(dir, '.git'))) {
    const parent = path.dirname(dir);
    if (parent === dir) {
      throw new Error(`no checkout above ${fileURLToPath(import.meta.url)}`);
    }
    dir = parent;
  }
  return path.join(dir, relative);
}
