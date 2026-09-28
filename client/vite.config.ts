import { svelte } from '@sveltejs/vite-plugin-svelte';
import { defineConfig } from 'vite';

// 1420 is the port the Tauri shell points `devUrl` at, so it is fixed
// rather than chosen at runtime: a dev server that moves breaks the shell.
export default defineConfig({
  plugins: [svelte()],
  server: { port: 1420, strictPort: true },
});
