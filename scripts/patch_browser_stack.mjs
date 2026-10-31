#!/usr/bin/env node
// Give the vendored @playwright/mcp the surface forge needs and upstream does
// not provide.
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
// **A name per tab that costs no renderer.** Playwright addresses tabs by
// position and prints no identity; a close renumbers every later tab, so a
// hand-off number or an owner record keyed by position would silently point
// at another page. The CDP target id is the name the browser and forge's
// registry share, and it is only readable through a page-domain CDP call, so
// the patch reads it once per page, bounded, caches it on the tab, and adds
// one machine line to the tab listing; the service strips that line before
// the agent sees the listing.
//
// **Selection by that name.** The select arm takes a CDP target instead of a
// position, and reads the target again when a page's cached read failed, so a
// page paused at the wrong moment heals on its own call.
//
// **A listing that reads.** See the tab-list patch below: the list arm must
// not make a page, because forge lists from a history watch.
//
// **Pinned and exact**: each needle is the pinned @playwright/mcp 0.0.83
// bundle's own excerpt, checked in beside this file. A version bump that
// changes any of them fails here, at install time, rather than silently
// shipping a driver that never surfaces a chooser, never names a tab or
// creates one on every listing.
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

const TABSELECT_NEEDLE = readFileSync(
  new URL("./playwright-mcp-0.0.83-tabselect.txt", import.meta.url),
  "utf8",
).replace(/\n$/, "");

const TABSCHEMA_NEEDLE = readFileSync(
  new URL("./playwright-mcp-0.0.83-tabschema.txt", import.meta.url),
  "utf8",
).replace(/\n$/, "");

const DIALOG_NEEDLE = readFileSync(
  new URL("./playwright-mcp-0.0.83-dialoganswer.txt", import.meta.url),
  "utf8",
).replace(/\n$/, "");

// **A page dialog's CEF callback is the host's**, so the page's own CDP
// session cannot answer it: `Page.handleJavaScriptDialog` reports "No dialog
// is showing" whether or not a host handler exists (measured 2026-11-01).
// The agent's answer therefore goes to forge over the driver's socket, and
// the host answers the callback it already holds. A socket that cannot be
// reached falls back to the CDP call, which is what an unpatched forge did.
const DIALOG_REPLACEMENT = [
  "      handle: async (tab2, params2, response2) => {",
  "        const dialogState = tab2.modalStates().find((state) => state.type === \"dialog\");",
  "        if (!dialogState)",
  '          throw new Error("No dialog visible");',
  "        tab2.clearModalState(dialogState);",
  "        // FORGE_DIALOG_ANSWER 1: the host owns the dialog's CEF callback.",
  "        let forgeAnswered = false;",
  "        try {",
  "          const forgeSocket = process.env.FORGE_DIALOG_SOCKET;",
  "          if (forgeSocket) {",
  '            const forgeNet = require("net");',
  "            const forgeLine = JSON.stringify({",
  "              accept: params2.accept,",
  "              promptText: params2.promptText",
  "            });",
  "            await new Promise((resolve, reject) => {",
  "              const forgeClient = forgeNet.createConnection(forgeSocket);",
  '              forgeClient.on("error", reject);',
  '              forgeClient.end(forgeLine + "\\n", resolve);',
  "            });",
  "            forgeAnswered = true;",
  "          }",
  "        } catch (forgeWhy) {}",
  "        await tab2.waitForCompletion(async () => {",
  "          if (forgeAnswered) return;",
  "          if (params2.accept)",
  "            await dialogState.dialog.accept(params2.promptText);",
  "          else",
  "            await dialogState.dialog.dismiss();",
  "        });",
  "      },",
].join("\n");

// **A tab is selected by its target, never by its position.** The position
// renumbers on every close and can only be learned from a listing; the CDP
// target is what the browser names every page by and what forge's registry
// already holds, so a call selects its tab with no listing at all (#2732).
//
// **A miss is a page the last read could not name, and this arm heals it.**
// The service no longer lists the driver on the call path, so a page that was
// paused when its target was read would never be named by another listing;
// the selection itself is where the second, bounded read goes.
const TABSELECT_REPLACEMENT = [
  "          case \"select\": {",
  "            // FORGE_SELECT_TARGET 2: a target names the page, not a position.",
  "            if (params2.target !== void 0) {",
  "              let forgeAt = context.tabs().findIndex((tab2) => tab2.forgeTarget === params2.target);",
  "              if (forgeAt < 0) {",
  "                // A page whose target read timed out is read again here, so",
  "                // a tab paused while the driver attached heals on its own call.",
  "                for (const tab2 of context.tabs()) {",
  '                  if (tab2.forgeTarget !== "") continue;',
  "                  try {",
  "                    const forgeTimeout = () =>",
  '                      new Promise((_, reject) => setTimeout(() => reject(new Error("forge target read timed out")), 1000));',
  "                    const forgeSession = await Promise.race([",
  "                      tab2.page.context().newCDPSession(tab2.page),",
  "                      forgeTimeout()",
  "                    ]);",
  "                    const forgeInfo = await Promise.race([",
  '                      forgeSession.send("Target.getTargetInfo"),',
  "                      forgeTimeout()",
  "                    ]);",
  '                    tab2.forgeTarget = (forgeInfo && forgeInfo.targetInfo && forgeInfo.targetInfo.targetId) || "";',
  "                    await forgeSession.detach().catch(() => {});",
  "                  } catch (forgeWhy) {",
  '                    tab2.forgeTarget = "";',
  "                  }",
  "                  tab2.forgeTargetAt = Date.now();",
  "                  if (tab2.forgeTarget === params2.target) {",
  "                    forgeAt = context.tabs().indexOf(tab2);",
  "                    break;",
  "                  }",
  "                }",
  "              }",
  "              if (forgeAt < 0)",
  '                throw new Error(`Tab ${params2.target} is not one the driver can name`);',
  "              await context.selectTab(forgeAt);",
  "              break;",
  "            }",
  "            if (params2.index === void 0)",
  '              throw new Error("Tab index is required");',
  "            await context.selectTab(params2.index);",
  "            break;",
  "          }",
].join("\n");

/// The version-1 select arm, rewritten in place on an existing stack.
const OLD_TABSELECT_REPLACEMENT = [
  "          case \"select\": {",
  "            // FORGE_SELECT_TARGET 1: a target names the page, not a position.",
  "            if (params2.target !== void 0) {",
  "              const forgeAt = context.tabs().findIndex((tab2) => tab2.forgeTarget === params2.target);",
  "              if (forgeAt < 0)",
  '                throw new Error(`Tab ${params2.target} is not one the driver can name`);',
  "              await context.selectTab(forgeAt);",
  "              break;",
  "            }",
  "            if (params2.index === void 0)",
  '              throw new Error("Tab index is required");',
  "            await context.selectTab(params2.index);",
  "            break;",
  "          }",
].join("\n");

const TABSCHEMA_REPLACEMENT = [
  '          index: z25.number().optional().describe("Tab index, used for close/select. If omitted for close, current tab is closed."),',
  "          // FORGE_SELECT_TARGET_SCHEMA 1: forge names a tab by its CDP target.",
  '          target: z25.string().optional().describe("CDP target id of the tab to select."),',
  '          url: z25.string().optional().describe("URL to navigate to in the new tab, used for new.")',
].join("\n");

// **A listing is a read of memory, never of a renderer.** `Target.getTargetInfo`
// is a page-domain call: a page paused on a `confirm()` (or a page whose script
// stopped) holds it for ever, and the old listing attached a CDP session to
// every tab on every call (#2732). The target id is therefore read once per
// page and cached on the tab, bounded on both legs, and the row itself is
// built from what the driver already holds.
const TAB_REPLACEMENT = [
  "        const forgeOpenTabs = context.tabs();",
  "        // FORGE_TAB_IDS_PATCH 3: the target id is cached per page, and a",
  "        // listing reads no renderer. A fill that timed out is retried after",
  "        // a window, so a page that was paused when the driver attached heals.",
  "        const forgeRows = [];",
  "        for (let i = 0; i < forgeOpenTabs.length; i++) {",
  "          const forgeTab = forgeOpenTabs[i];",
  "          const forgeStale =",
  "            forgeTab.forgeTarget === void 0 ||",
  '            (forgeTab.forgeTarget === "" && Date.now() - (forgeTab.forgeTargetAt || 0) > 5000);',
  "          if (forgeStale) {",
  "            let forgeTarget = null;",
  "            try {",
  "              const forgeTimeout = () =>",
  '                new Promise((_, reject) => setTimeout(() => reject(new Error("forge target read timed out")), 1000));',
  "              const forgeSession = await Promise.race([",
  "                forgeTab.page.context().newCDPSession(forgeTab.page),",
  "                forgeTimeout()",
  "              ]);",
  "              const forgeInfo = await Promise.race([",
  '                forgeSession.send("Target.getTargetInfo"),',
  "                forgeTimeout()",
  "              ]);",
  '              forgeTarget = (forgeInfo && forgeInfo.targetInfo && forgeInfo.targetInfo.targetId) || "";',
  "              await forgeSession.detach().catch(() => {});",
  "            } catch (forgeWhy) {",
  '              forgeTarget = "";',
  "            }",
  "            forgeTab.forgeTarget = forgeTarget;",
  "            forgeTab.forgeTargetAt = Date.now();",
  "          }",
  "          forgeRows.push({",
  "            i,",
  "            target: forgeTab.forgeTarget,",
  "            title: (forgeTab._lastHeader && forgeTab._lastHeader.title) || \"\",",
  "            url: forgeTab.page.url(),",
  "            current: forgeTab.isCurrentTab()",
  "          });",
  "        }",
  '        response2.addTextResult("FORGE-TABS " + JSON.stringify(forgeRows) + "\\n");',
].join("\n");

/// The version-2 replacement, rewritten in place on an existing stack.
const OLD_TAB_REPLACEMENT_V2 = [
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
    marker: "FORGE_TAB_IDS_PATCH 3",
    needle: TAB_NEEDLE,
    replacement: TAB_REPLACEMENT,
    old: [OLD_TAB_REPLACEMENT_V2, OLD_TAB_REPLACEMENT],
    missing:
      "the pinned Playwright bundle does not carry the tab listing this patch " +
      "expects. Bump scripts/patch_browser_stack.mjs with the @playwright/mcp version.",
    twice: "the tab listing appears more than once; refusing to guess.",
  },
  {
    label: "tab select by target",
    marker: "FORGE_SELECT_TARGET 2",
    needle: TABSELECT_NEEDLE,
    replacement: TABSELECT_REPLACEMENT,
    old: OLD_TABSELECT_REPLACEMENT,
    missing:
      "the pinned Playwright bundle does not carry the select arm this patch " +
      "expects. Bump scripts/patch_browser_stack.mjs with the @playwright/mcp version.",
    twice: "the select arm appears more than once; refusing to guess.",
  },
  {
    label: "tab select target field",
    marker: "FORGE_SELECT_TARGET_SCHEMA 1",
    needle: TABSCHEMA_NEEDLE,
    replacement: TABSCHEMA_REPLACEMENT,
    missing:
      "the pinned Playwright bundle does not carry the tab schema this patch " +
      "expects. Bump scripts/patch_browser_stack.mjs with the @playwright/mcp version.",
    twice: "the tab schema appears more than once; refusing to guess.",
  },
  {
    label: "dialog answer",
    marker: "FORGE_DIALOG_ANSWER 1",
    needle: DIALOG_NEEDLE,
    replacement: DIALOG_REPLACEMENT,
    missing:
      "the pinned Playwright bundle does not carry the dialog tool this patch " +
      "expects. Bump scripts/patch_browser_stack.mjs with the @playwright/mcp version.",
    twice: "the dialog tool appears more than once; refusing to guess.",
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
if (args[0] === "--print-tabselect-needle") {
  process.stdout.write(`${TABSELECT_NEEDLE}\n`);
  process.exit(0);
}
if (args[0] === "--print-tabschema-needle") {
  process.stdout.write(`${TABSCHEMA_NEEDLE}\n`);
  process.exit(0);
}
if (args[0] === "--print-dialog-needle") {
  process.stdout.write(`${DIALOG_NEEDLE}\n`);
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
  const olds = patch.old === undefined ? [] : [patch.old].flat();
  const old = olds.find((old) => patched.includes(old));
  if (old !== undefined) {
    patched = patched.replace(old, patch.replacement);
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
