#!/usr/bin/env node
// **A paused renderer must not hold a tab listing.** The version-2 listing
// attached a CDP session to every tab and asked it for its target id; a page
// paused on a `confirm()`, or one whose script stopped, holds that call for
// ever, so every session's listing hung behind it and forge ended the healthy
// host (#2732). This runs the listing against stub tabs - one whose CDP
// attach never settles on its first read, one healthy - and fails unless both
// rows come back within the bound, the paused one carrying no target. It also
// pins the per-page cache (a second listing reads no renderer, a read after
// the window retries once), selection by target, and the select arm's own
// bounded re-read of a page whose first read timed out.
//
// Usage: check_browser_stack_tabs.mjs <stack dir>
//        check_browser_stack_tabs.mjs --bundle <coreBundle.js>
import { existsSync, readFileSync } from "node:fs";
import { createRequire } from "node:module";
import { join } from "node:path";

// **Well under the bound a call is given**, so a listing that waits on the
// paused page fails here instead of looking like a slow answer.
const BOUND_MS = 5000;
// The patch retries a failed read only after this window (see the patch).
const RETRY_AFTER_MS = 5000;

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
const byTarget = patched.includes("FORGE_SELECT_TARGET");

const problems = [];
let attaches = 0;
let pausedAttempts = 0;
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

// The paused page's FIRST attach never settles: exactly what a renderer
// stopped on a dialog does to `Target.attachToTarget`. Its later reads
// answer, which is what the retry and the select arm's heal are for.
const paused = tab("T1", false);
const healthy = tab("T2", true);
context = {
  ensureBrowserContext: async () => context,
  tabs: () => [paused, healthy],
  newCDPSession: async (page) => {
    attaches += 1;
    if (page === paused.page) {
      pausedAttempts += 1;
      if (pausedAttempts === 1) return new Promise(() => {});
      return {
        send: async (method) =>
          method === "Target.getTargetInfo"
            ? { targetInfo: { targetId: "T1" } }
            : {},
        detach: async () => {},
      };
    }
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
  setIncludeSnapshot() {},
};

const timeout = new Promise((resolve) =>
  setTimeout(() => resolve("timeout"), BOUND_MS),
);

/// Run one listing and answer its parsed rows, or null when it did not answer.
const list = async () => {
  text = "";
  const listed = await Promise.race([
    tool.handle(context, { action: "list" }, response).then(() => "answered"),
    timeout,
  ]);
  if (listed === "timeout") return null;
  const exportLine = text
    .split("\n")
    .find((line) => line.startsWith("FORGE-TABS "));
  if (!exportLine) return [];
  return JSON.parse(exportLine.slice("FORGE-TABS ".length));
};

const rows = await list();
if (rows === null) {
  problems.push(
    `the listing did not answer within ${BOUND_MS} ms: the paused page held it`,
  );
} else {
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

// **The cache is the point**: a second listing inside the retry window reads
// no renderer, so the paused page costs one bounded read, not one per listing.
if (problems.length === 0) {
  const before = attaches;
  await list();
  if (attaches !== before) {
    problems.push(
      `a second listing attached to ${attaches - before} more page(s); the target read must be cached`,
    );
  }
}

// **A failed read is retried after the window**, on the next listing.
if (problems.length === 0 && byTarget) {
  paused.forgeTargetAt = Date.now() - RETRY_AFTER_MS - 1000;
  const before = attaches;
  const healed = await list();
  if (attaches !== before + 1) {
    problems.push(
      `a listing after the retry window attached ${attaches - before} time(s), expected 1`,
    );
  }
  const pausedRow = (healed || []).find((row) => row.i === 0) || {};
  if (pausedRow.target !== "T1") {
    problems.push(
      `the retried read left the paused page as ${JSON.stringify(pausedRow.target)}, expected T1`,
    );
  }
}

// **Selection is by target**, and a target no page carries is refused.
if (problems.length === 0 && byTarget) {
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

// **A select whose page the cache cannot name reads it again**, bounded, and
// drives it on that same call: the service no longer lists the driver before
// a call, so this is the only place the heal can happen.
if (problems.length === 0 && byTarget) {
  paused.forgeTarget = "";
  paused.forgeTargetAt = Date.now();
  const before = attaches;
  selected = [];
  let healed;
  try {
    healed = await Promise.race([
      tool.handle(context, { action: "select", target: "T1" }, response).then(
        () => "answered",
      ),
      timeout,
    ]);
  } catch (why) {
    problems.push(`a select with an empty cache threw: ${why}`);
  }
  if (healed === "timeout") {
    problems.push("a select with an empty cache did not answer within the bound");
  }
  if (attaches !== before + 1) {
    problems.push(
      `a select with an empty cache attached ${attaches - before} time(s), expected its own bounded re-read`,
    );
  }
  if (selected.length !== 1 || selected[0] !== 0) {
    problems.push(
      `a select with an empty cache drove ${JSON.stringify(selected)}, expected [0]`,
    );
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
    `the paused page carried no target, ${attaches} attach(es) over the run` +
    (byTarget ? ", selection by target resolves and heals" : ""),
);
