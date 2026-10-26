#!/usr/bin/env node
// Give the vendored @playwright/mcp the three things forge needs and
// upstream does not provide.
//
// **A file chooser surface.** Playwright owns the page's file chooser: its
// tab attaches a `filechooser` listener, which enables
// `Page.setInterceptFileChooserDialog` and keeps CEF's own OnFileDialog from
// ever firing. The host therefore never sees a page's chooser while the
// driver is attached. This patch leaves the listener in place and writes one
// line to the socket the driver bound, so the host can ask the controlling
// person or answer with the agent's own paths - both through the SAME
// captured chooser the MCP's `browser_file_upload` tool answers.
//
// **A stable name per tab.** Playwright addresses tabs by position and prints
// no identity; a close renumbers every later tab, so a hand-off number or an
// owner record keyed by position would silently point at another page. The
// CDP target id is the only stable name a page has, and only Playwright's
// internal page object carries it (`page.delegate._targetId`). The patch adds
// one machine line to the tab listing with each tab's target id; the service
// strips that line before the agent sees the listing.
//
// **A listing that reads.** See the tab-list patch below: the list arm must
// not make a page, because forge lists from a history watch.
//
// **Pinned and exact**: each needle is the pinned @playwright/mcp 0.0.83
// bundle's own excerpt, checked in beside this file. A version bump that
// changes any of them fails here, at install time, rather than silently
// shipping a driver that never surfaces a chooser, never names a tab or
// creates one on every listing. The borrowed private field is named by the
// tab patch's marker, so a bump's failure points at it.
//
// Usage: patch_browser_stack.mjs <stack dir>
//        patch_browser_stack.mjs --bundle <coreBundle.js>
//        patch_browser_stack.mjs --print-needle
//        patch_browser_stack.mjs --print-tabs-needle
import { existsSync, readFileSync, renameSync, unlinkSync, writeFileSync } from "node:fs";
import { join } from "node:path";

// Each needle is the checked-in excerpt for the pinned @playwright/mcp
// version, not a string this file can drift from: a version bump replaces the
// fixture and the vendor script's pin together.
const NEEDLE = readFileSync(
  new URL("./playwright-mcp-0.0.83-filechooser.txt", import.meta.url),
  "utf8",
).replace(/\n$/, "");

const REPLACEMENT = [
  '          eventsHelper.addEventListener(p, "filechooser", (chooser) => {',
  "            this.setModalState({",
  '              type: "fileChooser",',
  '              description: "File chooser",',
  "              fileChooser: chooser,",
  '              clearedBy: { tool: uploadFile.schema.name, skill: "upload" }',
  "            });",
  "            // FORGE_FILE_CHOOSER_PATCH 2: the host decides who answers this chooser.",
  "            // FORGE_FILE_CHOOSER_SOCKET is the driver's, and the line carries",
  "            // only what the host needs to ask a person.",
  "            try {",
  "              const forgeSocket = process.env.FORGE_FILE_CHOOSER_SOCKET;",
  "              if (forgeSocket) {",
  '                const forgeNet = require("net");',
  "                const forgeClient = forgeNet.createConnection(forgeSocket);",
  '                forgeClient.on("error", () => {});',
  '                forgeClient.end(JSON.stringify({ multiple: chooser.isMultiple(), current: this === this.context.currentTab(), url: p.url().slice(0, 512) }) + "\\n");',
  "              }",
  "            } catch (forgeWhy) {}",
  "          }),",
].join("\n");

const TAB_NEEDLE = readFileSync(
  new URL("./playwright-mcp-0.0.83-tabids.txt", import.meta.url),
  "utf8",
).replace(/\n$/, "");

const TABLIST_NEEDLE = readFileSync(
  new URL("./playwright-mcp-0.0.83-tablist.txt", import.meta.url),
  "utf8",
).replace(/\n$/, "");

// **A page guid survives a navigation; a CEF target id does not.** The
// service keys its tabs by the guid and reads the target id afresh on every
// listing, because CEF swaps it when the page navigates and CDP input is
// addressed by it.
const TAB_REPLACEMENT = [
  "        const forgeOpenTabs = context.tabs();",
  "        const tabHeaders = await Promise.all(forgeOpenTabs.map((tab2) => tab2.headerSnapshot()));",
  "        const result2 = renderTabsMarkdown(tabHeaders);",
  "        // FORGE_TAB_IDS_PATCH 2: the guid names the tab, the target id is",
  "        // where CDP drives it right now.",
  "        const forgeRows = [];",
  "        for (let i = 0; i < forgeOpenTabs.length; i++) {",
  "          const forgeTab = forgeOpenTabs[i];",
  '          let forgeTarget = "";',
  "          try {",
  "            const forgeSession = await forgeTab.page.context().newCDPSession(forgeTab.page);",
  '            const forgeInfo = await forgeSession.send("Target.getTargetInfo");',
  '            forgeTarget = (forgeInfo && forgeInfo.targetInfo && forgeInfo.targetInfo.targetId) || "";',
  "            await forgeSession.detach().catch(() => {});",
  "          } catch (forgeWhy) {",
  '            forgeTarget = "";',
  "          }",
  "          forgeRows.push({",
  "            i,",
  '            id: forgeTab.page._guid || "",',
  "            target: forgeTarget,",
  "            title: tabHeaders[i].title,",
  "            url: tabHeaders[i].url,",
  "            current: tabHeaders[i].current",
  "          });",
  "        }",
  '        response2.addTextResult("FORGE-TABS " + JSON.stringify(forgeRows) + "\\n" + result2.join("\\n"));',
].join("\n");

/// The version-1 replacement, rewritten in place on an existing stack.
const OLD_TAB_REPLACEMENT = [
  "        const forgeOpenTabs = context.tabs();",
  "        const tabHeaders = await Promise.all(forgeOpenTabs.map((tab2) => tab2.headerSnapshot()));",
  "        const result2 = renderTabsMarkdown(tabHeaders);",
  "        // FORGE_TAB_IDS_PATCH 1: forge's service needs a stable name per tab;",
  "        // Playwright's index renumbers on every close.",
  "        const forgeTabs = forgeOpenTabs.map((tab2, i) => ({",
  "          i,",
  '          id: (tab2.page && tab2.page.delegate && tab2.page.delegate._targetId) || "",',
  "          title: tabHeaders[i].title,",
  "          url: tabHeaders[i].url,",
  "          current: tabHeaders[i].current",
  "        }));",
  '        response2.addTextResult("FORGE-TABS " + JSON.stringify(forgeTabs) + "\\n" + result2.join("\\n"));',
].join("\n");

// **A stack vendored before the current file chooser patch version carries
// the version-1 replacement**, which the marker check would miss; it is
// rewritten in place so a patch update reaches an existing install, not only
// a fresh one.
const OLD_REPLACEMENT = [
  '          eventsHelper.addEventListener(p, "filechooser", (chooser) => {',
  "            this.setModalState({",
  '              type: "fileChooser",',
  '              description: "File chooser",',
  "              fileChooser: chooser,",
  '              clearedBy: { tool: uploadFile.schema.name, skill: "upload" }',
  "            });",
  "            // FORGE_FILE_CHOOSER_SOCKET: the host decides who answers this",
  "            // chooser. The socket is the driver's, and the line carries only",
  "            // what the host needs to ask a person.",
  "            try {",
  "              const forgeSocket = process.env.FORGE_FILE_CHOOSER_SOCKET;",
  "              if (forgeSocket) {",
  '                const forgeNet = require("net");',
  "                const forgeClient = forgeNet.createConnection(forgeSocket);",
  '                forgeClient.on("error", () => {});',
  '                forgeClient.end(JSON.stringify({ multiple: chooser.isMultiple(), url: p.url() }) + "\\n");',
  "              }",
  "            } catch (forgeWhy) {}",
  "          }),",
].join("\n");

// **A tab listing reads, it does not create.** Upstream's `list` arm calls
// `ensureTab()`, which calls `context.newPage()` whenever the driver has no
// current tab. Measured on this host with every page closed: `newPage()`
// throws after CEF has already made the browser, so the driver gets no tab
// at all, the target and its renderer stay, and every listing makes another
// (#2581). `ensureBrowserContext` keeps the chooser arming and creates
// nothing; scripts/check_browser_stack_list.mjs checks the behaviour.
const TABLIST_REPLACEMENT = [
  "        switch (params2.action) {",
  '          case "list": {',
  "            // FORGE_TAB_LIST_READS 1: differs from upstream - a listing reads, it never creates a page (#2581).",
  "            await context.ensureBrowserContext();",
  "            break;",
  "          }",
].join("\n");

const PATCHES = [
  {
    label: "file chooser",
    marker: "FORGE_FILE_CHOOSER_PATCH 2",
    needle: NEEDLE,
    replacement: REPLACEMENT,
    old: OLD_REPLACEMENT,
    missing:
      "the pinned Playwright bundle does not carry the file chooser listener this " +
      "patch expects. Bump scripts/patch_browser_stack.mjs with the @playwright/mcp version.",
    twice: "the file chooser listener appears more than once; refusing to guess.",
  },
  {
    label: "tab ids",
    marker: "FORGE_TAB_IDS_PATCH 2",
    needle: TAB_NEEDLE,
    replacement: TAB_REPLACEMENT,
    old: OLD_TAB_REPLACEMENT,
    missing:
      "the pinned Playwright bundle does not carry the tab listing this patch " +
      "expects (page._guid and a Target.getTargetInfo session). Bump " +
      "scripts/patch_browser_stack.mjs with the @playwright/mcp version.",
    twice: "the tab listing appears more than once; refusing to guess.",
  },
  {
    label: "tab list reads",
    marker: "FORGE_TAB_LIST_READS 1",
    needle: TABLIST_NEEDLE,
    replacement: TABLIST_REPLACEMENT,
    missing:
      "the pinned Playwright bundle does not carry the tab list arm this patch " +
      "expects (ensureTab). Bump scripts/patch_browser_stack.mjs with the " +
      "@playwright/mcp version.",
    twice: "the tab list arm appears more than once; refusing to guess.",
  },
];

const args = process.argv.slice(2);
if (args[0] === "--print-needle") {
  process.stdout.write(`${NEEDLE}\n`);
  process.exit(0);
}
if (args[0] === "--print-tabs-needle") {
  process.stdout.write(`${TAB_NEEDLE}\n`);
  process.exit(0);
}
if (args[0] === "--print-tablist-needle") {
  process.stdout.write(`${TABLIST_NEEDLE}\n`);
  process.exit(0);
}
if (args[0] === "--print-old") {
  process.stdout.write(`${OLD_REPLACEMENT}\n`);
  process.exit(0);
}

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
    "usage: patch_browser_stack.mjs <stack dir> | --bundle <coreBundle.js>",
  );
  process.exit(2);
}

if (!existsSync(bundle)) {
  console.error(`[ERROR] no Playwright bundle at ${bundle}`);
  process.exit(1);
}

const source = readFileSync(bundle, "utf8");
let patched = source;
const pending = [];
// **Every patch or none**: a bundle is written once, after every needle has
// been found, and each success is printed only once that write has landed -
// a missing needle must not leave a log claiming patches a file never got.
for (const patch of PATCHES) {
  if (patched.includes(patch.marker)) {
    console.log(`[..] the ${patch.label} patch is already applied (${bundle})`);
    continue;
  }
  if (patch.old !== undefined && patched.includes(patch.old)) {
    patched = patched.replace(patch.old, patch.replacement);
    pending.push(`[OK] the ${patch.label} patch is updated (${bundle})`);
    continue;
  }
  const at = patched.indexOf(patch.needle);
  if (at < 0) {
    console.error(`[ERROR] ${patch.missing}`);
    process.exit(1);
  }
  if (patched.indexOf(patch.needle, at + 1) >= 0) {
    console.error(`[ERROR] ${patch.twice}`);
    process.exit(1);
  }
  patched = patched.replace(patch.needle, patch.replacement);
  pending.push(`[OK] the ${patch.label} patch is applied (${bundle})`);
}
// **Written beside the bundle and renamed**, so a torn write cannot leave
// marker-bearing bytes that every later run reads as already applied.
if (patched !== source) {
  const temporary = `${bundle}.forge-patch-${process.pid}`;
  try {
    writeFileSync(temporary, patched);
    renameSync(temporary, bundle);
  } catch (why) {
    try {
      unlinkSync(temporary);
    } catch {}
    console.error(`[ERROR] the patched bundle could not be written: ${why}`);
    process.exit(1);
  }
}
for (const line of pending) console.log(line);
