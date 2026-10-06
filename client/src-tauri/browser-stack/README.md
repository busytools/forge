# The vendored browser stack

What the client's browser host runs, bundled inside the app so nothing
downloads at first use and an offline machine needs nothing. Everything
here except this README is produced by `just vendor-browser-stack`, which
pins the three artifacts, verifies their hashes and unpacks them:

| Path | What it is |
|---|---|
| `node/bin/node` | Node LTS, `bin/node` alone - the rest of the node tree is npm, which nothing here needs |
| `playwright-mcp/node_modules/@playwright/mcp/` | `@playwright/mcp`, the driver the host speaks MCP to, with `playwright-core` beside it and no browsers downloaded (the host points it at the bundled Chromium's CDP endpoint) |
| `browser/chrome-mac-arm64/` | Chrome for Testing, the FULL build - a visible window is a switch the host can flip, so the headless shell will not do |

`bundle.resources` in `tauri.conf.json` carries `browser-stack/**/*`, so the
whole tree lands under `Contents/Resources/browser-stack/` in the bundle and
the host resolves it from the resource directory. The directory is
gitignored; this file is the one tracked thing in it, which is also what
keeps the resource glob matching something in a checkout that has never
vendored.

The three pins (versions and SHA-256) live in `scripts/vendor_browser_stack.sh`,
and `just vendor-browser-stack` prints them, so a release log names the
stack it shipped. Bumping any of the three is an edit there plus one client
release.
