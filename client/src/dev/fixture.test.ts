import { execSync } from 'node:child_process';
import { existsSync, readFileSync, readdirSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

import { describe, expect, it } from 'vitest';

const CLIENT = fileURLToPath(new URL('../..', import.meta.url));

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
 * A string only the fixture carries. It is the org every record in it names,
 * and no page, component or wire type spells it: a marker that appeared in
 * the source would make this pass for the wrong reason.
 */
const MARKER = 'TestOrg';

describe('the shipped bundle', () => {
  /**
   * **The app's only input is the server URL.** A shell that quietly draws
   * bundled data when a server is absent is the failure this prevents, and
   * it is the kind that looks like success: a static import defeats
   * tree-shaking, so a dev-only route behind a guard still ships the fixture
   * unless the import itself is deferred.
   */
  it('carries no fixture', () => {
    // `NODE_ENV=production` explicitly: vitest sets it to `test`, and Vite
    // reads that as a DEVELOPMENT build, which would put the fixture back
    // through the DEV guard and fail this on a dev bundle rather than on the
    // one that ships.
    execSync('npm run build', {
      cwd: CLIENT,
      stdio: 'pipe',
      env: { ...process.env, NODE_ENV: 'production' },
    });
    const bundle = filesUnder(path.join(CLIENT, 'dist'))
      .filter((file) => /\.(js|css|html)$/.test(file))
      .map((file) => readFileSync(file, 'utf8'))
      .join('\n');

    expect(bundle.length, 'the build produced nothing to read').toBeGreaterThan(0);
    expect(bundle, 'the fixture reached the production bundle').not.toContain(MARKER);

    // The tab's mark and the link to it: a build that loses either ships a
    // dangling link with nothing else failing.
    const dist = path.join(CLIENT, 'dist');
    expect(existsSync(path.join(dist, 'favicon.png')), 'the built bundle carries no favicon').toBe(
      true,
    );
    expect(
      readFileSync(path.join(dist, 'index.html'), 'utf8'),
      'the built page carries no link to the mark',
    ).toContain('/favicon.png');
  }, 60_000);
});
