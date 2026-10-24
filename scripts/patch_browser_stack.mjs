#!/usr/bin/env node
// Give the vendored @playwright/mcp one way to surface a file chooser.
//
// **Playwright owns the page's file chooser**: its tab attaches a
// `filechooser` listener, which enables `Page.setInterceptFileChooserDialog`
// and keeps CEF's own OnFileDialog from ever firing. The host therefore never
// sees a page's chooser while the driver is attached. This patch leaves the
// listener in place and writes one line to the socket the driver bound, so
// the host can ask the controlling person or answer with the agent's own
// paths - both through the SAME captured chooser the MCP's
// `browser_file_upload` tool answers.
//
// **Pinned and exact**: the needle is the pinned @playwright/mcp 0.0.83
// bundle's listener. A version bump that changes it fails here, at install
// time, rather than silently shipping a driver that never surfaces a chooser.
//
// Usage: patch_browser_stack.mjs <stack dir>
//        patch_browser_stack.mjs --bundle <coreBundle.js>
//        patch_browser_stack.mjs --print-needle
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";

// The needle is the checked-in excerpt for the pinned @playwright/mcp
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

const MARKER = "FORGE_FILE_CHOOSER_PATCH 2";

// **A stack vendored before this patch version carries the version-1
// replacement**, which the marker check would miss; it is rewritten in place
// so a patch update reaches an existing install, not only a fresh one.
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

const args = process.argv.slice(2);
if (args[0] === "--print-needle") {
  process.stdout.write(`${NEEDLE}\n`);
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
if (source.includes(MARKER)) {
  console.log(`[..] the file chooser patch is already applied (${bundle})`);
  process.exit(0);
}
// An older patch is brought up to this one before the original-listener
// check, because its listener is already gone.
const oldAt = source.indexOf(OLD_REPLACEMENT);
if (oldAt >= 0) {
  writeFileSync(bundle, source.replace(OLD_REPLACEMENT, REPLACEMENT));
  console.log(`[OK] the file chooser patch is updated (${bundle})`);
  process.exit(0);
}
const at = source.indexOf(NEEDLE);
if (at < 0) {
  console.error(
    "[ERROR] the pinned Playwright bundle does not carry the file chooser listener this " +
      "patch expects. Bump scripts/patch_browser_stack.mjs with the @playwright/mcp version.",
  );
  process.exit(1);
}
if (source.indexOf(NEEDLE, at + 1) >= 0) {
  console.error(
    "[ERROR] the file chooser listener appears more than once; refusing to guess.",
  );
  process.exit(1);
}
writeFileSync(bundle, source.replace(NEEDLE, REPLACEMENT));
console.log(`[OK] the file chooser patch is applied (${bundle})`);
