import { readFileSync } from 'node:fs';

import { svelte } from '@sveltejs/vite-plugin-svelte';
import { defineConfig } from 'vite';
import { configDefaults } from 'vitest/config';

/**
 * The release `just release` sets on the client's own manifest.
 *
 * Baked in as a define because the built app has no manifest to read, and a
 * client that could not name its own release could name only one half of a
 * protocol skew.
 */
function clientRelease(): string {
  const manifest = readFileSync(new URL('./src-tauri/Cargo.toml', import.meta.url), 'utf8');
  const declared = /^version = "([^"]+)"/m.exec(manifest);
  if (declared === null || declared[1] === undefined) {
    throw new Error(
      'client/src-tauri/Cargo.toml declares no `version`, so the client cannot name its own release',
    );
  }
  return declared[1];
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
