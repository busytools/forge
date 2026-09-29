import { svelte } from '@sveltejs/vite-plugin-svelte';
import { defineConfig } from 'vite';

// 1420 is the port the Tauri shell points `devUrl` at, so it is fixed
// rather than chosen at runtime: a dev server that moves breaks the shell.
export default defineConfig(({ mode }) => ({
  plugins: [svelte()],
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
}));
