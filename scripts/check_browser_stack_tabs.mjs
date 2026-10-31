#!/usr/bin/env node
// **A paused renderer must not hold a tab listing.** The version-2 listing
// attached a CDP session to every tab and asked it for its target id; a page
// paused on a `confirm()`, or one whose script stopped, holds that call for
// ever, so every session's listing hung behind it and forge ended the healthy
// host (#2732). This runs the listing against stub tabs - one whose CDP
// attach never settles, one healthy - and fails unless both rows come back
// within the bound, the paused one carrying no target. It also pins the
// per-page cache (a second listing reads no renderer) and, on a bundle that
// carries it, selection by target.
//
// Usage: check_browser_stack_tabs.mjs <stack dir>
//        check_browser_stack_tabs.mjs --bundle <coreBundle.js>
import { existsSync, readFileSync } from "node:fs";
import { createRequire } from "node:module";
import { join } from "node:path";

// **Well under the bound a call is given**, so a listing that waits on the
// paused page fails here instead of looking like a slow answer.
const BOUND_MS = 5000;
// The patch's own per-page read timeout, plus room for a loaded machine.
const SETTLE_MS = 3000;

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
    "usage: check_browser_stack_tabs.mjs <stack dir> | --bundle <coreBundle.js>",
  );
  process.exit(2);
}
if (!existsSync(bundle)) {
  console.error(`[ERROR] no Playwright bundle at ${bundle}`);
  process.exit(1);
}

const require = createRequire(import.meta.url);
const { tools } = require(bundle);
const tool = tools.browserTools.find((t) => t.schema.name === "browser_tabs");
if (!tool) {
  console.error(
    `[ERROR] the stack has no browser_tabs tool (${bundle}); refusing to guess ` +
      "at a shape this check does not know.",
  );
  process.exit(1);
}

const patched = readFileSync(bundle, "utf8");
const byTarget = patched.includes("FORGE_SELECT_TARGET 1");

const problems = [];
let attaches = 0;
let selected = [];
let context;

const tab = (target, current) => {
  const page = {
    url: () => `http://${target}/`,
    context: () => context,
  };
  return {
    page,
    _lastHeader: { title: target, url: page.url(), current },
    headerSnapshot: async () => ({ title: target, url: page.url(), current }),
    isCurrentTab: () => current,
  };
};

// The paused page's attach never settles: exactly what a renderer stopped on
// a dialog does to `Target.attachToTarget`.
const paused = tab("T1", false);
const healthy = tab("T2", true);
context = {
  ensureBrowserContext: async () => context,
  tabs: () => [paused, healthy],
  newCDPSession: async (page) => {
    attaches += 1;
    if (page === paused.page) return new Promise(() => {});
    return {
      send: async (method) =>
        method === "Target.getTargetInfo"
          ? { targetInfo: { targetId: "T2" } }
          : {},
      detach: async () => {},
    };
  },
  selectTab: async (index) => {
    selected.push(index);
    return context.tabs()[index];
  },
};

let text = "";
const response = {
  addAction() {},
  addCode() {},
  addResult() {},
  addTextResult(value) {
    text += value;
  },
  // Version 2's arm called headerSnapshot directly, so this stub is not what
  // it needs; an arm that asks the response for a snapshot still works.
  setIncludeSnapshot() {},
};

const timeout = new Promise((resolve) =>
  setTimeout(() => resolve("timeout"), BOUND_MS),
);

const listed = await Promise.race([
  tool.handle(context, { action: "list" }, response).then(() => "answered"),
  timeout,
]);
if (listed === "timeout") {
  problems.push(
    `the listing did not answer within ${BOUND_MS} ms: the paused page held it`,
  );
} else {
  const exportLine = text
    .split("\n")
    .find((line) => line.startsWith("FORGE-TABS "));
  if (!exportLine) {
    problems.push("the listing carried no FORGE-TABS export");
  } else {
    const rows = JSON.parse(exportLine.slice("FORGE-TABS ".length));
    if (rows.length !== 2) {
      problems.push(`the listing named ${rows.length} tabs, expected 2`);
    }
    const pausedRow = rows.find((row) => row.i === 0) || {};
    const healthyRow = rows.find((row) => row.i === 1) || {};
    if (pausedRow.target !== "") {
      problems.push(
        `the paused page was given a target (${JSON.stringify(pausedRow.target)})`,
      );
    }
    if (healthyRow.target !== "T2") {
      problems.push(
        `the healthy page's target is ${JSON.stringify(healthyRow.target)}, expected T2`,
      );
    }
    if (!healthyRow.current) {
      problems.push("the healthy page's row lost its current flag");
    }
  }
  // **The cache is the point**: a second listing reads no renderer, so the
  // paused page costs one bounded read per driver, not one per listing.
  const before = attaches;
  await Promise.race([
    tool.handle(context, { action: "list" }, response).then(() => "answered"),
    timeout,
  ]);
  if (attaches !== before) {
    problems.push(
      `a second listing attached to ${attaches - before} more page(s); the target read must be cached`,
    );
  }
}

if (byTarget) {
  selected = [];
  await tool.handle(context, { action: "select", target: "T2" }, response);
  if (selected.length !== 1 || selected[0] !== 1) {
    problems.push(
      `selection by target drove ${JSON.stringify(selected)}, expected [1]`,
    );
  }
  let refused = false;
  try {
    await tool.handle(context, { action: "select", target: "T9" }, response);
  } catch {
    refused = true;
  }
  if (!refused) {
    problems.push("a target no page carries was selected instead of refused");
  }
}

if (problems.length > 0) {
  console.error(
    `[ERROR] the vendored tab listing is not a read of memory: ${problems.join("; ")}. ` +
      "A listing that waits on a page's CDP session is one paused renderer " +
      "holding every session's every call (#2732).",
  );
  process.exit(1);
}
console.log(
  `[OK] the vendored tab listing reads memory: 2 tabs answered within ${BOUND_MS} ms, ` +
    `the paused page carried no target, ${attaches} attach(es) over two listings` +
    (byTarget ? ", selection by target resolves" : ""),
);
