# Vendored web assets

Served from the process rather than fetched from a CDN, so a running forge
needs no network for its own page. Byte-for-byte as published; nothing here
is patched.

| File | Package | Version | Licence |
|---|---|---|---|
| `htmx.min.js` | [htmx.org](https://www.npmjs.com/package/htmx.org) | 2.0.11 | 0BSD |
| `htmx-sse.min.js` | [htmx-ext-sse](https://www.npmjs.com/package/htmx-ext-sse) | 2.2.4 | 0BSD |
| `idiomorph-ext.min.js` | [idiomorph](https://www.npmjs.com/package/idiomorph) | 0.8.0 | 0BSD |
| `fonts/InterVariable.woff2` | [Inter](https://github.com/rsms/inter) | 4.1 | OFL 1.1 |
| `fonts/FiraCode-Regular.woff2` | [Fira Code](https://github.com/tonsky/FiraCode) | 6.2 | OFL 1.1 |
| `fonts/FiraCode-Medium.woff2` | [Fira Code](https://github.com/tonsky/FiraCode) | 6.2 | OFL 1.1 |

All three scripts are Zero-Clause BSD: use, copy, modify and distribute
for any purpose, with or without fee. Each package ships its own copy of
the licence text in its tarball.

Both faces are SIL Open Font License 1.1, which travels with the font:
`fonts/Inter-LICENSE.txt` and `fonts/FiraCode-LICENSE.txt` are the
upstream texts, byte for byte. Inter is a variable font over `wght
100-900` with an `opsz` axis, and the `font-weight: 100 900` in its
`@font-face` is what lets the type scale reach a weight between the
static ones instead of snapping to a synthesized Bold. Fira Code is the
two weights the mockups ask Google Fonts for, regular and medium, which
is every weight the sheet asks of it. A rule wanting a third would get a
synthesized bold, which in a monospace is worse than either real weight.

## Where each came from, and how to update it

```
curl -sL https://registry.npmjs.org/htmx.org/-/htmx.org-<version>.tgz | tar xz
curl -sL https://registry.npmjs.org/htmx-ext-sse/-/htmx-ext-sse-<version>.tgz | tar xz
curl -sL https://registry.npmjs.org/idiomorph/-/idiomorph-<version>.tgz | tar xz

curl -sSL -o InterVariable.woff2 https://raw.githubusercontent.com/rsms/inter/v4.1/docs/font-files/InterVariable.woff2
curl -sSL -o Inter-LICENSE.txt  https://raw.githubusercontent.com/rsms/inter/v4.1/LICENSE.txt
curl -sSL -o Fira_Code_v6.2.zip https://github.com/tonsky/FiraCode/releases/download/6.2/Fira_Code_v6.2.zip
unzip Fira_Code_v6.2.zip woff2/FiraCode-Regular.woff2 woff2/FiraCode-Medium.woff2
curl -sSL -o FiraCode-LICENSE.txt https://raw.githubusercontent.com/tonsky/FiraCode/6.2/LICENSE
```

| Vendored as | Taken from |
|---|---|
| `htmx.min.js` | `htmx.org/package/dist/htmx.min.js` |
| `htmx-sse.min.js` | `htmx-ext-sse/package/dist/sse.min.js` |
| `idiomorph-ext.min.js` | `idiomorph/package/dist/idiomorph-ext.min.js` |
| `fonts/InterVariable.woff2` | `rsms/inter` at tag `v4.1`, `docs/font-files/InterVariable.woff2` |
| `fonts/Inter-LICENSE.txt` | the same tag, `LICENSE.txt` |
| `fonts/FiraCode-Regular.woff2` | `Fira_Code_v6.2.zip`, `woff2/FiraCode-Regular.woff2` |
| `fonts/FiraCode-Medium.woff2` | `Fira_Code_v6.2.zip`, `woff2/FiraCode-Medium.woff2` |
| `fonts/FiraCode-LICENSE.txt` | `tonsky/FiraCode` at tag `6.2`, `LICENSE` |

Inter is taken from the repository rather than from a subsetting service
on purpose. The Fontsource and Google Fonts latin subsets drop seven
marks the full font carries (`↩ ↳ ⏎ ⇧ ● ▼` among them), so the smaller
file is bought with glyphs that fall back. Its upstream is paused rather
than maintained - the last commit to the font files is the `v4.1` tag - so
treat this file as final rather than waiting on a release.

`"Inter"` is a Reserved Font Name, declared in upstream's README rather
than in the licence text vendored here, so a subset or a rename of the
352KB file needs a name of its own.

`idiomorph-ext.min.js` is self-contained (it defines `Idiomorph` and
registers the htmx `morph` swaps), so `idiomorph.min.js` is not needed
alongside it.

htmx 2 also ships `dist/ext/sse.js`, which is the **htmx 1** extension and
warns as much at load. `htmx-sse.min.js` is the separate 2.x package.

## Hashes

```
98a46496de0c3605fbffdce9167ba427bdd9553184f83f149c261891a92c0136  htmx-sse.min.js
d6fdc75f204e6bdefa99b69bf1e6d4ac69b8a364f77929f45c13476b4000f717  htmx.min.js
5811b9a7eda14878b7dab4378bf60432e1bb6f11fcfb970cc26540642650181e  idiomorph-ext.min.js
693b77d4f32ee9b8bfc995589b5fad5e99adf2832738661f5402f9978429a8e3  fonts/InterVariable.woff2
262481e844521b326f5ecd053e59b98c8b2da78c8ee1bdbb6e8174305e54935a  fonts/Inter-LICENSE.txt
a6ce59520b90e15d7062ffef214f94c8add5a4085c0bbb1683602ef227a4d1fe  fonts/FiraCode-Regular.woff2
0e04bafb989ea46e840a581e49557b229662a00021493a5744c595d0882adf28  fonts/FiraCode-Medium.woff2
1d41e10031ab125302780a05ec4c91d218e47db0c7e37cf315cce5e608cdc25c  fonts/FiraCode-LICENSE.txt
```

Verify with `shasum -a 256 crates/forge-web/assets/*.js crates/forge-web/assets/fonts/*`.

## What the page asks of them

One region, one event, one swap:

```
body   hx-ext="sse, morph" sse-connect="/events" sse-close="close"
#fleet sse-swap="fleet" hx-swap="morph:outerHTML" hx-target="#home"
#home  (the region the stream sends - no wiring of its own)
```

Two things here are load-bearing, and both were found by measuring rather
than by reading:

- **Both extension names.** An undeclared swap style is not an error in
  htmx: it falls back to filling the target, which nests the region inside
  itself on the first event.
- **The swap lives on a wrapper the payload never replaces.** htmx
  re-processes whatever it swaps in, so a `sse-swap` on the region itself
  registers one more listener for every event. Measured in a browser: 43
  swaps for the first update and climbing, ~90 by the fiftieth, against one
  per event with the wrapper. A page that grows its own work exponentially
  is worse than a page that does not update.

The stream's payload is the region element itself, so the swap replaces it
rather than filling it. `morph:outerHTML` keeps the swap from resetting DOM
state inside the region; the page has none today, and the rule from the
spike is that no swap target may contain the composer or a `<details>`,
which is where that would start to matter.
