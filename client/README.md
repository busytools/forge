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

## Four reads the home snapshot does not carry

`crates/forge-web/src/home.rs` draws cells the home snapshot cannot fill,
so the client leaves them empty rather than guessing at them. Each is a
read the server would have to add; none is a client defect.

| Cell it feeds | What is missing |
|---|---|
| `.row .where` | the per-row working tree. `home.rs` reads it from `WorkCache` per row, and `HomeWire` carries no work - the twentieth record has the working tree for a *session*, not for a home row. |
| `.row .what`, and `.st` with it | tasks. `HomeWire` carries none, so a row cannot show the task it holds, its status chip, or its artifact link. The header's task total goes with it. |
| the refusal line | `would_bind`. `has_model` crosses, so `no model declared` is drawn; `no usable accounts` needs whether an account would bind, which does not cross. |
| the header's version | forge's own version. `home.rs` draws `env!("CARGO_PKG_VERSION")` and nothing on the wire carries it. The claude version and the update notice both do, and are drawn. |

A fifth is a view state rather than a read: the **unseen** mark. The
server's `Live` owns it and does not encode it yet, so the client draws the
state when it is handed one and computes none itself.
