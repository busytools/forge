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

## One URL is two pages until the store lands

`/` is the home, and the app opens on `/connect` and rewrites the URL to
match, so a cold load of `/` lands on the door. The not-found page's own
link points at `/`, which routes to the home. So the same URL is two pages
depending on how you arrived, and a reload flips it. The fix is to gate the
home on a connection, which is part of wiring the pages up.
