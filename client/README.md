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
  card, the lifecycle mark, the brand mark, the icon sprite. One theme, one
  copy of each.
- `src/theme.ts` - the palettes and typeface stacks the names resolve to.
  The greeting carries the NAMES; the values live here, and there is no
  client-side reader of `forge.toml` by any path.
- `src/assets/` - `web.css`, `sprite.svg` and the fonts, taken from
  `crates/forge-web` before that crate is deleted.

`forge.toml` is the server's, and the client never reads it: `[web] mark`,
`[web] theme` and `[web] font` arrive in the greeting. `ClientSettings`
carries a name for each, not a value.

## Four reads the home snapshot does not carry

`crates/forge-web/src/home.rs` draws cells the home snapshot cannot fill,
so the client leaves them empty rather than guessing at them. Each is a
read the server would have to add; none is a client defect.

| Cell it feeds | What is missing |
|---|---|
| `.row .where` | the per-row working tree. `home.rs` reads it from `WorkCache` per row, and `HomeWire` carries no work - the twentieth record has the working tree for a *session*, not for a home row. |
| `.row .what`, and `.st` with it | tasks. `HomeWire` carries none, so a row cannot show the task it holds, its status chip, or its artifact link. The header's task total goes with it. |
| the refusal line | `would_bind`. `has_model` crosses, so `no model declared` is drawn; `no usable accounts` needs whether an account would bind, which does not cross. Until it is wired, a row draws the same middot whether a spawn would be allowed or the answer is unknown. |
| the header's version | forge's own version. `home.rs` draws `env!("CARGO_PKG_VERSION")` and nothing on the wire carries it. The claude version and the update notice both do, and are drawn. |

A fifth is a view state rather than a read: the **unseen** mark. The
server's `Live` owns it and does not encode it yet, so the client draws the
state when it is handed one and computes none itself.

## Two the snapshot carries and nothing draws

`service_status` and `fatal_error` DO cross on the home subject, and no
page reads either. So a forge that is exiting or that failed at startup
draws as a healthy fleet. Dropped on purpose for now rather than by
oversight: both are a view of their own, and the home's band is not where
they belong.

## One URL is two pages until the store lands

`/` is the home, and the app opens on `/connect` and rewrites the URL to
match, so a cold load of `/` lands on the door. The not-found page's own
link points at `/`, which routes to the home. So the same URL is two pages
depending on how you arrived, and a reload flips it. The fix belongs with
the socket: gate the home on a connection once a store holds one.

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
