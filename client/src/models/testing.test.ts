import { readFileSync, readdirSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

import { describe, expect, it } from 'vitest';

/** The client's source root, so the sweep does not depend on the process cwd. */
const SRC = fileURLToPath(new URL('..', import.meta.url));

/** Every file under `dir`, at any depth. */
function filesUnder(dir: string): string[] {
  const found: string[] = [];
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) found.push(...filesUnder(full));
    else found.push(full);
  }
  return found;
}

/**
 * **The models fixture is imported by tests, and nothing else.**
 *
 * `fixture.test.ts` builds the bundle and fails on a fixture reaching it by a
 * MARKER string, and `models/testing.ts` carries none - the models wire names
 * no org - so the bundle guard cannot see this one. Its own rule ("nothing
 * the shipped app imports may import this file") is checked here instead: a
 * shipped import of `modelsWire` would put mock models in the app, which is
 * the failure the app's one-input rule exists to prevent.
 */
describe('the models fixture', () => {
  it('is imported by tests alone', () => {
    const shipped: string[] = [];
    const testFiles: string[] = [];

    for (const file of filesUnder(SRC)) {
      if (!/\.(ts|svelte)$/.test(file)) continue;
      const where = path.relative(SRC, file);
      if (path.basename(file) === 'testing.ts') continue;
      const text = readFileSync(file, 'utf8');
      // A sibling `./testing` import means THIS module only from a file under
      // `models/`: every other directory has a `testing` of its own, and a
      // bare pattern would report the composer's harness as an offender.
      const importsIt =
        /from '[^']*\/models\/testing'/.test(text) ||
        (where.startsWith(`models${path.sep}`) && /from '\.\/testing'/.test(text));
      if (!importsIt) continue;
      // A `reference` or `svelte` file is source; a test importing it is the
      // only intended reader.
      if (file.endsWith('.test.ts')) testFiles.push(where);
      else shipped.push(where);
    }

    expect(shipped, 'a shipped file imports the models fixture').toEqual([]);
    // And the sweep really reads: a pattern that matched nothing would pass
    // the check above for ever, which is a green that means nothing.
    expect(testFiles.length, 'no test imports the fixture, so the sweep is blind').toBeGreaterThan(
      0,
    );
  });
});
