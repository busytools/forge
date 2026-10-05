/// <reference types="svelte" />
/// <reference types="vite/client" />

/**
 * The client's own release, baked at build time from
 * `client/src-tauri/Cargo.toml` - the number `just release` sets on the
 * client half.
 */
declare const __FORGE_CLIENT_VERSION__: string;
