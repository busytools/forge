import { execFileSync } from 'node:child_process';
import { readFileSync } from 'node:fs';

import { svelte } from '@sveltejs/vite-plugin-svelte';
import { defineConfig } from 'vite';
import { configDefaults } from 'vitest/config';

/**
 * The release `just release` sets on the client's own manifest, stamped
 * with the commit it was built from.
 *
 * Baked in as a define because the built app has no manifest to read, and a
 * client that could not name its own release could name only one half of a
 * protocol skew. **The `+<short sha>` suffix is the server's own stamp's
 * shape** (its build script's `FORGE_BUILD_SUFFIX_SHORT`), because the
 * footer draws the two builds side by side and one bare where the other
 * carried its commit read as two different kinds of fact. A checkout with
 * no git keeps the bare version, as the server's stamp does.
 */
function clientRelease(): string {
  const manifest = readFileSync(new URL('./src-tauri/Cargo.toml', import.meta.url), 'utf8');
  const declared = /^version = "([^"]+)"/m.exec(manifest);
  if (declared === null || declared[1] === undefined) {
    throw new Error(
      'client/src-tauri/Cargo.toml declares no `version`, so the client cannot name its own release',
    );
  }
  try {
    const sha = execFileSync('git', ['rev-parse', '--short', 'HEAD'], {
      cwd: new URL('.', import.meta.url),
      encoding: 'utf8',
    }).trim();
    return sha === '' ? declared[1] : `${declared[1]}+${sha}`;
  } catch {
    return declared[1];
  }
}

// 1420 is the port the Tauri shell points `devUrl` at, so it is fixed
// rather than chosen at runtime: a dev server that moves breaks the shell.
export default defineConfig(({ mode }) => ({
  plugins: [svelte()],
  define: { __FORGE_CLIENT_VERSION__: JSON.stringify(clientRelease()) },
  server: { port: 1420, strictPort: true },
  // **Test mode only.** A test run asks for `svelte` with node's conditions,
  // which resolves its SERVER build, so `mount` throws - and the shell is the
  // one component a test has to mount rather than render, because its work
  // happens in a launch and not in a render. Scoped to the test mode so the
  // bundle that ships resolves exactly as it did before.
  //
  // It REPLACES Vite's defaults rather than adding to them, so test mode also
  // drops `module` and `development|production`. Harmless while nothing keys
  // on either, and broader than it looks if something ever does.
  ...(mode === 'test' ? { resolve: { conditions: ['browser'] } } : {}),
  // A crashed mutation run leaves its sandbox copy under `.stryker-tmp`, and
  // without this a later vitest run collects the copies beside the originals.
  test: { exclude: [...configDefaults.exclude, '**/.stryker-tmp/**'] },
}));
