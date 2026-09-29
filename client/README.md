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

`just client-dev` runs the app itself in development: the window over that
dev server, reloading on a frontend edit, with nothing installed and no
bundle built.

## The desktop shell

`src-tauri/` is a Tauri 2 app that wraps the built bundle. The crate is
its own cargo workspace root, so none of the repo's gates and neither the
Rust nor the JavaScript job in CI reaches it.

```sh
npm run tauri dev     # dev server plus the app window
npm run tauri build   # forge.app and a dmg, under src-tauri/target/release/bundle
```

The production build embeds `dist/`, so it needs `npm run build` to have
run and fails if that output is missing. The dev window loads the dev
server instead and needs no `dist/`. `just client-tauri-check` is the one
to run before handing a change here over: it is the production build with
`--locked`.

Two traps sit between a hand-typed command and a working one, and that
recipe carries the working form. The CLI reads `CI` from the environment
as its own boolean `--ci` flag, so a `CI` holding anything but `true` or
`false` - `0` is what forge's sessions set - stops the build before it
starts; passing `--ci` explicitly overrides it. And `npm` swallows a bare
`--`, so a cargo flag has to arrive as `run tauri -- build -- <flags>`.

A start has no terminal to report to, so it writes to
`~/Library/Logs/dev.vedhavyas.forge/forge.log` instead. That file is
named after the product rather than the crate. `forge client started` is
the line a start that reached the window leaves, so what follows it, or
its absence, is what explains a bounce.

The icon is the `panes` mark from `src/brand.ts`, on the `--bg` ground in
the `--accent` colour. Render the mark to a 1024 PNG and pass that to
`npx tauri icon`: the committed `icons/icon.png` is a 512 the generator
writes and will not reproduce the `.icns` on its own. That file stays
because `generate_context!` reads it at compile time, and with the five
`bundle.icon` names it is all of the set worth committing.

The version is this crate's own rather than the workspace's. The client is
a different program from the server, so `just release` does not bump it.

## What is here

- `src/wire/` - the server's shapes, copied from the fixtures on
  `origin/forge-server-socket`. The pages type their props against these,
  so the two halves cannot disagree about what a home looks like.
- `src/components/` - the pieces every page draws with: the row, the band
  card, the lifecycle mark, the brand mark, the icon sprite. One theme, one
  copy of each.

**The sprite is inlined by the shell on every page and nothing yet points at
it.** No page in the base slice draws an icon, so `Icon.svelte` has no
caller and removing `<Sprite />` from `Shell.svelte` would break nothing a
test sees. It is here because the plan's salvage set names it and because
the session and composer tasks run in parallel and cannot edit this shared
base. The first icon belongs to Task 5's inspector, and that is where the
sprite stops being dead weight.

- `src/theme.ts` - the palettes and typeface stacks the names resolve to.
  The greeting carries the NAMES; the values live here, and there is no
  client-side reader of `forge.toml` by any path.
- `src/assets/` - `web.css`, `sprite.svg` and the fonts, taken from
  `crates/forge-web` before that crate is deleted.

`forge.toml` is the server's, and the client never reads it: `[web] mark`,
`[web] theme` and `[web] font` arrive in the greeting. `ClientSettings`
carries a name for each, not a value.

## Every cell the server's home draws

The four reads this file used to list as missing now cross on a project
row, and the home draws all four: `work` fills `.row .where`, `tasks`
fills `.row .what` with its status chip and its artifact link, `would_bind`
decides the refusal's second arm, and `forge_version_short` carries the
header's version. The fixture's `projects[]` entries are
`{project, work, tasks, crons, would_bind, chip}`; `crates/forge-web/src/home.rs`
is still the port's source for the markup.

**The unseen marks cross too**, as `unseen` - a list of slots. The server's
`Live` owns that fact and a client cannot reconstruct it from the records,
so it is carried rather than derived, and the client draws the state when
it is handed one.

## Two the snapshot carries and nothing draws

`service_status` and `fatal_error` DO cross on the home subject, and no
page reads either. So a forge that is exiting or that failed at startup
draws as a healthy fleet. Dropped on purpose for now rather than by
oversight: both are a view of their own, and the home's band is not where
they belong.

Two more ride the home row and no page in this slice draws them: `crons`,
which is the inspector's schedules section, and `chip`, which is the
account a row binds. Both belong to pages that do not exist yet.

## Where a cold load lands

The app opens on the address it last connected to, which it keeps in the
webview's own store. `src/connect/remembered.ts` is the store,
`src/connect/boot.ts` is the launch: read the address, try it, and land on
the home when something answers. When nothing does, or when there is nothing
remembered, the door comes back carrying the address that was tried and the
reason it did not answer.

A launch that finds nothing to open on rewrites `/` to `/connect`, which is
the door's own URL. A launch that tried an address and failed stays at `/`,
so a reload retries rather than being told the address was wrong. What is
kept is the address that last WORKED - a failed one is not forgotten, since
that is the one a person is about to fix.

`/fixture` and a session deep link do not launch: they are addressed at
something, and opening a socket behind them would change what they draw.
