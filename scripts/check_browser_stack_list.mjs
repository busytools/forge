#!/usr/bin/env node
// **The vendored tab listing must not create a page.** Upstream's
// `browser_tabs` list arm calls `context.ensureTab()`, which calls
// `context.newPage()` whenever the driver has no current tab; forge lists
// from its history watch every 500 ms, so a closed tab turns that into one
// page and one CEF renderer per tick (#2581). This runs the arm against a
// stub context and counts what it makes, so a re-vendor or an
// `@playwright/mcp` bump that brings the write back fails the vendor step
// instead of reaching a running browser.
//
// Usage: check_browser_stack_list.mjs <stack dir>
//        check_browser_stack_list.mjs --bundle <coreBundle.js>
import { existsSync } from "node:fs";
import { createRequire } from "node:module";
import { join } from "node:path";

const ROUNDS = 3;

const args = process.argv.slice(2);
let bundle;
if (args[0] === "--bundle" && args[1]) {
  bundle = args[1];
} else if (args[0] && !args[0].startsWith("-")) {
  bundle = join(
    args[0],
    "playwright-mcp/node_modules/playwright-core/lib/coreBundle.js",
  );
} else {
  console.error(
    "usage: check_browser_stack_list.mjs <stack dir> | --bundle <coreBundle.js>",
  );
  process.exit(2);
}
if (!existsSync(bundle)) {
  console.error(`[ERROR] no Playwright bundle at ${bundle}`);
  process.exit(1);
}

const require = createRequire(import.meta.url);
const { tools } = require(bundle);
const listing = tools.browserTools.find(
  (tool) => tool.schema.name === "browser_tabs",
);
if (!listing) {
  console.error(
    `[ERROR] the stack has no browser_tabs tool (${bundle}); refusing to ` +
      "guess at a shape this check does not know.",
  );
  process.exit(1);
}

// The listing needs only a context to build and a tab list to read; every
// page-making door records instead of making anything.
let contexts = 0;
const made = { ensureTab: 0, newPage: 0 };
const context = {
  ensureBrowserContext: async () => {
    contexts += 1;
    return context;
  },
  // Upstream's arm calls this, and with no current tab it opens one.
  ensureTab: async () => {
    made.ensureTab += 1;
    await context.newTab();
  },
  newTab: async () => {
    made.newPage += 1;
  },
  tabs: () => [],
};
const response = {
  addAction() {},
  addCode() {},
  addTextResult() {},
  setIncludeSnapshot() {},
};

for (let round = 0; round < ROUNDS; round += 1) {
  await listing.handle(context, { action: "list" }, response);
}

const problems = [];
if (contexts !== ROUNDS) {
  problems.push(
    `it built the driver's context ${contexts} times, expected ${ROUNDS} on the patched arm`,
  );
}
if (made.ensureTab !== 0) {
  problems.push(`it ran ensureTab ${made.ensureTab} times`);
}
if (made.newPage !== 0) {
  problems.push(`it created ${made.newPage} pages`);
}
if (problems.length > 0) {
  console.error(
    `[ERROR] the vendored tab listing is not a read: ${problems.join("; ")}. ` +
      "A listing that creates a page makes one renderer per history-watch tick (#2581).",
  );
  process.exit(1);
}
console.log(
  `[OK] the vendored tab listing reads: ${ROUNDS} listings, ${contexts} context builds, 0 pages`,
);
