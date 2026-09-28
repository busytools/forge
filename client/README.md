# forge client

The app that connects to a running forge and draws it. Svelte 5 on Vite,
with a Tauri shell for the desktop.

It is not a second forge. Every client connects to a forge someone else
started, and the socket carries everything the pages draw.

## Running it

```sh
npm install
npm run dev      # dev server on http://localhost:1420
npm run build    # dist/
npm run test     # vitest
npm run typecheck
```

`1420` is fixed rather than defaulted, because the Tauri shell points its
`devUrl` at it.

## What is here

- `src/wire/` - the server's shapes, copied from the fixtures on
  `origin/forge-server-socket`. The pages type their props against these,
  so the two halves cannot disagree about what a home looks like.
- `src/components/` - the pieces every page draws with: the row, the band
  card, the lifecycle mark. One theme, one copy of each.
- `src/theme.ts` - the tokens the server sends, applied as CSS variables.
- `src/assets/` - `web.css` and the fonts, taken from `crates/forge-web`
  before that crate is deleted.

`forge.toml` is the server's, and the client never reads it: `[web] mark`,
`[web] theme` and `[web] font` arrive in the greeting.
