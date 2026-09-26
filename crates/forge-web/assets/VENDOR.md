# Vendored web assets

Served from the process rather than fetched from a CDN, so a running forge
needs no network for its own page. Byte-for-byte as published; nothing here
is patched.

| File | Package | Version | Licence |
|---|---|---|---|
| `htmx.min.js` | [htmx.org](https://www.npmjs.com/package/htmx.org) | 2.0.11 | 0BSD |
| `htmx-sse.min.js` | [htmx-ext-sse](https://www.npmjs.com/package/htmx-ext-sse) | 2.2.4 | 0BSD |
| `idiomorph-ext.min.js` | [idiomorph](https://www.npmjs.com/package/idiomorph) | 0.8.0 | 0BSD |

All three are Zero-Clause BSD: use, copy, modify and distribute for any
purpose, with or without fee. Each package ships its own copy of the
licence text in its tarball.

## Where each came from, and how to update it

```
curl -sL https://registry.npmjs.org/htmx.org/-/htmx.org-<version>.tgz | tar xz
curl -sL https://registry.npmjs.org/htmx-ext-sse/-/htmx-ext-sse-<version>.tgz | tar xz
curl -sL https://registry.npmjs.org/idiomorph/-/idiomorph-<version>.tgz | tar xz
```

| Vendored as | Taken from |
|---|---|
| `htmx.min.js` | `htmx.org/package/dist/htmx.min.js` |
| `htmx-sse.min.js` | `htmx-ext-sse/package/dist/sse.min.js` |
| `idiomorph-ext.min.js` | `idiomorph/package/dist/idiomorph-ext.min.js` |

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
```

Verify with `shasum -a 256 crates/forge-web/assets/*.js`.

## What the page asks of them

One region, one event, one swap:

```
body  hx-ext="sse" sse-connect="/events" sse-close="close"
#home sse-swap="fleet" hx-swap="morph:outerHTML"
```

The stream's payload is the region element itself, so the swap replaces it
rather than filling it. `morph:outerHTML` keeps the swap from resetting DOM
state inside the region; the page has none today, and the rule from the
spike is that no swap target may contain the composer or a `<details>`,
which is where that would start to matter.
