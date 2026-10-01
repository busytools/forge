#!/usr/bin/env node
/**
 * The density instrument, as one command: `node scripts/density/measure.mjs`.
 *
 * Issue #1509 compares the client's markdown against the terminal's and this
 * produces the numbers that comparison needs. Two halves, one table:
 *
 * - **client** - the fixture rendered by the real `renderProse`, in a page
 *   linking the real `web.css`, measured in a headless browser the script
 *   starts and stops itself. Read through CDP rather than `--dump-dom`,
 *   because layout under `--dump-dom` is page-dependent and a probe that
 *   silently reports zero boxes reads as a page with no spacing at all.
 * - **terminal** - the same fixture through the real message render path in
 *   `forge-tui`, run as an `#[ignore]`d test and read back as rows.
 *
 * **Both halves are face-asserted.** The sheet declares no `--ui` / `--mono`
 * (the server injects them at runtime), so a probe page that supplies its own
 * stack measures the fallback and every number is wrong by the difference
 * between two faces. So the theme tokens and both stacks come from the real
 * `client/src/theme.ts`, the vendored woff2 sit beside the page, the face is
 * re-declared after the sheet link (an earlier `@font-face` loses to it), and
 * the run fails unless the measured advance is Fira Code's.
 *
 * The same page is then rendered a second time with no `@font-face` at all -
 * a negative control. If the two disagree on the advance, the instrument can
 * see the face; if they agree, it cannot and nothing here is trustworthy.
 */

import { spawn, spawnSync } from 'node:child_process';
import { copyFileSync, mkdirSync, readFileSync, readdirSync, writeFileSync } from 'node:fs';
import { homedir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const REPO = path.resolve(HERE, '..', '..');
const CLIENT = path.join(REPO, 'client');
const WORK = path.join('/tmp', 'forge-density');

/** The terminal renders at this many columns; the client is fitted to match. */
const COLUMNS = Number(process.env.DENSITY_COLUMNS ?? 100);

/** A wide window, for the reading that shows whether the measure is capped. */
const WIDE = Number(process.env.DENSITY_WIDE ?? 1600);

/**
 * Fira Code's advance in em, which the client's stack resolves to.
 *
 * Menlo - the `--mono` fallback - is 0.6021, a 2% difference that moves a
 * character boundary. The asserts below are what keep the two apart.
 */
const FIRA_ADVANCE_EM = 0.6154;

/**
 * A terminal row's height, in cell widths.
 *
 * **This is the one figure here that is not measured by this script**, and it
 * is the one that turns the terminal's rows into the same unit as the client's
 * pixels. A terminal row is as tall as the emulator draws it, so the TUI can
 * neither know nor report it; the value is read off Ved's own screenshots,
 * where the cell is 17 device px wide and a row 34 device px tall at 9:30.23
 * and 9:31.30. Multiply by Fira Code's advance (0.6154em) to get the row in em.
 *
 * A different terminal font size does not move this: the aspect is the
 * emulator's line spacing, not the font size.
 */
const CELL_ASPECT = 2.0;

/** The chromium the repo's own Playwright cache holds, newest first. */
function headlessShell() {
  const cache = path.join(homedir(), 'Library', 'Caches', 'ms-playwright');
  const builds = readdirSync(cache)
    .filter((name) => name.startsWith('chromium_headless_shell-'))
    .sort()
    .reverse();
  for (const build of builds) {
    const binary = path.join(cache, build, 'chrome-headless-shell-mac-arm64', 'chrome-headless-shell');
    try {
      readFileSync(binary);
      return binary;
    } catch {
      continue;
    }
  }
  throw new Error(`no chrome-headless-shell under ${cache}`);
}

/** A minimal CDP client over Node's own WebSocket. */
class Cdp {
  constructor(url) {
    this.url = url;
    this.next = 0;
    this.pending = new Map();
  }

  async open() {
    this.socket = new WebSocket(this.url);
    await new Promise((resolve, reject) => {
      this.socket.addEventListener('open', resolve, { once: true });
      this.socket.addEventListener('error', () => reject(new Error('cdp refused')), { once: true });
    });
    this.socket.addEventListener('message', (event) => {
      const message = JSON.parse(event.data);
      const settle = this.pending.get(message.id);
      if (settle !== undefined) {
        this.pending.delete(message.id);
        settle(message);
      }
    });
  }

  send(method, params = {}, sessionId) {
    const id = (this.next += 1);
    return new Promise((resolve, reject) => {
      this.pending.set(id, (message) =>
        message.error === undefined
          ? resolve(message.result)
          : reject(new Error(`${method}: ${message.error.message}`)),
      );
      this.socket.send(JSON.stringify({ id, method, params, ...(sessionId ? { sessionId } : {}) }));
    });
  }

  close() {
    this.socket.close();
  }
}

/**
 * Load `file` in the running browser and return what the probe measured.
 *
 * The probe is async (it waits on `document.fonts.ready`), so the wait is on
 * the returned value rather than on the load event: a page that has loaded
 * but has not yet swapped the fallback for the real face would measure the
 * fallback.
 */
async function readPage(cdp, sessionId, url, columns) {
  await cdp.send('Page.navigate', { url }, sessionId);
  for (let attempt = 0; attempt < 100; attempt += 1) {
    const probe = await cdp.send(
      'Runtime.evaluate',
      {
        expression: 'typeof window.__density === "function" ? "ready" : document.readyState',
        returnByValue: true,
      },
      sessionId,
    );
    if (probe.result.value === 'ready') break;
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  const measured = await cdp.send(
    'Runtime.evaluate',
    {
      expression: `window.__density(${columns})`,
      awaitPromise: true,
      returnByValue: true,
    },
    sessionId,
  );
  if (measured.exceptionDetails !== undefined) {
    throw new Error(`probe threw: ${measured.exceptionDetails.text}`);
  }
  return measured.result.value;
}

/** The fixture's prose, as the page draws it, through the real renderer. */
async function renderProse(fixture) {
  const vite = await import(path.join(CLIENT, 'node_modules', 'vite', 'dist', 'node', 'index.js'));
  const server = await vite.createServer({
    root: CLIENT,
    server: { middlewareMode: true },
    logLevel: 'error',
  });
  try {
    const prose = await server.ssrLoadModule('/src/chat/prose.ts');
    const theme = await server.ssrLoadModule('/src/theme.ts');
    return {
      html: prose.renderProse(fixture),
      tokens: theme.rootTokens('dark'),
      stack: theme.fontStack(null),
      fallback: theme.fontStack('system'),
    };
  } finally {
    await server.close();
  }
}

/** The inline `style` `applySettings` writes, as the attribute a page can carry. */
function rootStyle(tokens, stack) {
  const pairs = [...Object.entries(tokens), ['--ui', stack.ui], ['--mono', stack.mono]];
  return pairs.map(([name, value]) => `${name}:${value}`).join(';');
}

/** One probe page. `face` false is the negative control. */
function page({ prose, root, face }) {
  const faces = face
    ? `<style>
@font-face { font-family: "Fira Code"; src: url("fonts/FiraCode-Regular.woff2") format("woff2");
  font-weight: 400; font-style: normal; font-display: block; }
@font-face { font-family: "Fira Code"; src: url("fonts/FiraCode-Medium.woff2") format("woff2");
  font-weight: 500; font-style: normal; font-display: block; }
</style>`
    : '';
  // Single-quoted attribute: both stacks carry double quotes around the family
  // name, and a double-quoted attribute would end at the first one - which
  // leaves `--ui` undefined, the page drawing Times New Roman, and every
  // number here a measurement of the browser's default font.
  return `<!doctype html>
<html lang="en" style='${root}'>
<head>
<meta charset="utf-8">
<title>forge density probe</title>
<link rel="stylesheet" href="web.css">
${faces}
</head>
<body>
<div class="app left-hidden right-hidden">
  <main class="chat">
    <div class="conv">
      <div class="turn"><div class="work"><div class="prose">${prose}</div></div></div>
    </div>
  </main>
</div>
<script>
${readFileSync(path.join(HERE, 'probe.js'), 'utf8')}
</script>
</body>
</html>
`;
}

const CONTENT_TYPES = {
  '.html': 'text/html',
  '.css': 'text/css',
  '.js': 'text/javascript',
  '.woff2': 'font/woff2',
};

/**
 * Serve `WORK` over http.
 *
 * `file://` will not do: a web font is a CORS-governed subresource, and an
 * opaque origin has nothing to match, so the face stays `unloaded` and the
 * page measures the fallback while looking like it asked for the real one.
 */
async function serve(root, port) {
  const { createServer } = await import('node:http');
  const server = createServer((request, response) => {
    const requested = new URL(request.url, 'http://x').pathname;
    const file = path.join(root, requested === '/' ? 'index.html' : requested);
    // The woff2 sit one directory down, so this resolves the whole path rather
    // than a basename - and refuses anything that climbs back out of the root.
    if (!file.startsWith(`${root}${path.sep}`)) {
      response.writeHead(403).end();
      return;
    }
    try {
      const body = readFileSync(file);
      response.writeHead(200, {
        'content-type': CONTENT_TYPES[path.extname(file)] ?? 'application/octet-stream',
      });
      response.end(body);
    } catch {
      response.writeHead(404).end();
    }
  });
  await new Promise((resolve) => server.listen(port, '127.0.0.1', resolve));
  return server;
}

/**
 * The terminal half: render the fixture through the message path and read the
 * rows back.
 *
 * `--run-ignored ignored-only` rather than a filter alone, because the test is
 * `#[ignore]`d so that `just check` and CI never pay for it.
 */
function terminalRows(fixturePath, out, width) {
  const run = spawnSync(
    'cargo',
    [
      'nextest', 'run',
      '--run-ignored', 'ignored-only',
      '-p', 'forge-tui',
      '--lib',
      '-E', 'test(density_probe_writes_rows_json)',
      '--no-capture',
    ],
    {
      cwd: REPO,
      env: {
        ...process.env,
        FORGE_DENSITY_FIXTURE: fixturePath,
        FORGE_DENSITY_OUT: out,
        FORGE_DENSITY_WIDTH: String(width),
      },
      encoding: 'utf8',
    },
  );
  if (run.status !== 0) {
    process.stderr.write(run.stdout ?? '');
    process.stderr.write(run.stderr ?? '');
    throw new Error(`the terminal probe failed (${run.status})`);
  }
  return JSON.parse(readFileSync(out, 'utf8'));
}

/**
 * Every block and gap, side by side.
 *
 * **The two views share no pixel, so the ratio is taken in em** - the drawn
 * height divided by the block's own font size, on both sides. Dividing the
 * client by its line-height instead gives "lines", which cannot be set against
 * the terminal's rows: a row is as tall as the emulator draws it, and the two
 * views' fonts are different sizes to begin with. em is the one unit that
 * survives both.
 */
function table(client, terminal) {
  const rows = [];
  const blocks = client.blocks;
  const rowEm = CELL_ASPECT * FIRA_ADVANCE_EM;
  for (let index = 0; index < Math.max(blocks.length, terminal.bands.length); index += 1) {
    const cell = blocks[index] ?? null;
    const band = terminal.bands[index] ?? { drawn: 0, gap_before: 0, bold: false, label: '(none)' };
    const em = cell === null ? null : Number((cell.height / cell.fontSize).toFixed(2));
    const gapEm = cell === null || cell.gapAfter === null
      ? null
      : Number((cell.gapAfter / cell.fontSize).toFixed(2));
    const termEm = band.drawn * rowEm;
    rows.push({
      block: cell === null ? band.label : cell.cls === '' ? cell.tag : `${cell.tag}.${cell.cls}`,
      what: (cell === null ? band.label : cell.text).replaceAll(/\s+/g, ' ').slice(0, 30),
      'term rows': band.drawn,
      'term gap rows': band.gap_before,
      'term em': Number(termEm.toFixed(2)),
      'client px': cell === null ? null : cell.height,
      'client em': em,
      ratio: em === null || termEm === 0 ? null : Number((em / termEm).toFixed(2)),
      'client gap px': cell === null ? null : cell.gapAfter,
      'client gap em': gapEm,
      bold: `${band.bold} / ${cell === null ? '?' : cell.heaviest}`,
    });
  }
  return rows;
}

async function main() {
  mkdirSync(path.join(WORK, 'fonts'), { recursive: true });
  copyFileSync(path.join(CLIENT, 'src', 'assets', 'web.css'), path.join(WORK, 'web.css'));
  for (const font of ['FiraCode-Regular.woff2', 'FiraCode-Medium.woff2']) {
    copyFileSync(path.join(CLIENT, 'public', 'fonts', font), path.join(WORK, 'fonts', font));
  }

  const fixturePath = path.join(HERE, 'fixture.md');
  const fixture = readFileSync(fixturePath, 'utf8');
  const { html, tokens, stack, fallback } = await renderProse(fixture);
  const root = rootStyle(tokens, stack);
  // The control is the same page under `[web] font = "system"` - a setting
  // forge really has, and the one the original trap amounted to: same markup,
  // different face. If the two read the same advance, this instrument cannot
  // see the face and nothing it reports is worth anything.
  const controlRoot = rootStyle(tokens, fallback);

  writeFileSync(path.join(WORK, 'index.html'), page({ prose: html, root, face: true }));
  writeFileSync(path.join(WORK, 'control.html'), page({ prose: html, root: controlRoot, face: false }));

  const port = 8731;
  const server = await serve(WORK, port);
  const at = (name) => `http://127.0.0.1:${port}/${name}`;

  const shell = headlessShell();
  const browser = spawn(shell, [
    '--remote-debugging-port=0',
    '--no-first-run',
    '--no-default-browser-check',
    '--disable-gpu',
    'about:blank',
  ]);
  let endpoint = null;
  browser.stderr.setEncoding('utf8');
  browser.stderr.on('data', (chunk) => {
    const found = /DevTools listening on (ws:\/\/\S+)/.exec(chunk);
    if (found !== null) endpoint = found[1];
  });
  const deadline = Date.now() + 10_000;
  while (endpoint === null && Date.now() < deadline) {
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  if (endpoint === null) throw new Error('the headless browser never reported its endpoint');

  let client;
  let control;
  let wide;
  let terminal;
  try {
    const cdp = new Cdp(endpoint);
    await cdp.open();
    const { targetId } = await cdp.send('Target.createTarget', { url: 'about:blank' });
    const { sessionId } = await cdp.send('Target.attachToTarget', { targetId, flatten: true });
    await cdp.send('Page.enable', {}, sessionId);
    client = await readPage(cdp, sessionId, at('index.html'), COLUMNS);
    control = await readPage(cdp, sessionId, at('control.html'), COLUMNS);
    await cdp.send(
      'Emulation.setDeviceMetricsOverride',
      { width: WIDE, height: 1000, deviceScaleFactor: 1, mobile: false },
      sessionId,
    );
    wide = await readPage(cdp, sessionId, at('index.html'), null);
    cdp.close();
  } finally {
    browser.kill('SIGTERM');
    server.close();
  }

  terminal = terminalRows(fixturePath, path.join(WORK, 'terminal.json'), COLUMNS);

  const want = Number((FIRA_ADVANCE_EM * client.face.proseSize).toFixed(4));
  const faceOk = Math.abs(client.face.advance - want) < 0.05;
  const controlDiffers = Math.abs(control.face.advance - client.face.advance) > 0.01;
  // The face is asserted on the element the geometry was read from, not just
  // on the page: a stack that resolves on `<body>` and not on the block would
  // leave every height below measured in a different font.
  const stackCarries = client.blocks.every((block) => block.family.includes('Fira Code'));
  const columnOk = client.column.settled && client.column.fits === COLUMNS;
  // The advance alone cannot tell the vendored woff2 from a Fira Code the
  // machine happens to have installed, and this machine does - so the shipped
  // faces have to report `loaded` as well, or the run is measuring a local
  // face and calling it the shipped one.
  const facesLoaded = client.face.declared.every((face) => face.status === 'loaded');

  const cuts = [...new Set(client.face.declared.map((face) => Number(face.weight)))].sort();
  const heaviest = Math.max(
    ...[client.weight.strong, client.weight.th].filter((value) => value !== null).map(Number),
  );
  const clientPx =
    client.blocks.reduce((sum, block) => sum + block.height, 0)
    + client.blocks.reduce((sum, block) => sum + (block.gapAfter ?? 0), 0);
  const gapPx = client.blocks.reduce((sum, block) => sum + (block.gapAfter ?? 0), 0);
  const fontSize = client.blocks[0].fontSize;
  const clientEm = clientPx / fontSize;
  const rowEm = CELL_ASPECT * FIRA_ADVANCE_EM;
  const terminalEm = terminal.drawn_rows * rowEm;

  const say = (lines) => process.stdout.write(`\n${lines.join('\n')}\n`);
  const ink = client.weight.canvas
    .map((row) => `${row.weight}: ${row.ink} ink / ${row.width} advance`)
    .join('  |  ');

  say([
    `fixture            ${fixturePath}`,
    `terminal width     ${COLUMNS} columns`,
    `client column      ${client.column.contentWidth} px (fits ${client.column.fits} chars, `
      + `settled=${client.column.settled})`,
    `measure @ ${WIDE}px   prose fits ${wide.column.fits} chars in a ${wide.column.contentWidth} px `
      + `column (no cap in the sheet)`,
    `face (computed)    ${client.face.family}`,
    `face (canonical)   ${client.face.canon}`,
    `advance            ${client.face.advance} px at ${client.face.proseSize}px `
      + `(Fira Code ${want}, fallback ${control.face.advance})`,
    `prose size         ${client.face.proseSize}px, code ${client.face.codeSize}px`,
    `line-height        body ${client.lineHeight.body} | prose ${client.lineHeight.prose} `
      + `| table ${client.lineHeight.table} | code ${client.lineHeight.code}`,
    `weight             strong ${client.weight.strong} | p ${client.weight.prose} `
      + `| th ${client.weight.th}`,
    `faces declared     ${cuts.join(', ')} (Fira Code ships static cuts; a request for `
      + `${heaviest} has no cut and is ${heaviest > Math.max(...cuts) ? 'SYNTHESIZED' : 'real'})`,
    `canvas ink, 10x H  ${ink}`,
    `contrast           ${Object.entries(client.contrast)
      .map(([name, value]) =>
        value === null ? `${name}: absent` : `${name} ${value.ratio}:1 (${value.color} on ${value.ground})`,
      )
      .join('  |  ')}`,
  ]);

  const rows = table(client, terminal);
  const headers = Object.keys(rows[0]);
  const widths = headers.map((header) =>
    Math.max(header.length, ...rows.map((row) => String(row[header]).length)),
  );
  const line = (values) =>
    values.map((value, index) => String(value).padEnd(widths[index])).join('  ');
  say([
    line(headers),
    line(widths.map((width) => '-'.repeat(width))),
    ...rows.map((row) => line(headers.map((header) => row[header]))),
    '',
    `terminal           ${terminal.text_blocks} text blocks, logical rows ${terminal.logical_rows}, `
      + `drawn ${terminal.drawn_rows} rows`,
    `client             ${client.blocks.length} blocks, ${clientPx.toFixed(1)} px = `
      + `${clientEm.toFixed(1)} em (${fontSize}px text), ${(gapPx / fontSize).toFixed(1)} em of it `
      + `block gaps`,
    `whole message      terminal ${terminal.drawn_rows} rows x ${rowEm.toFixed(3)} em = `
      + `${terminalEm.toFixed(1)} em vs client ${clientEm.toFixed(1)} em `
      + `= ${(clientEm / terminalEm).toFixed(2)}x`,
  ]);

  const checks = [
    ['face advance', faceOk, `measured ${client.face.advance} vs Fira Code ${want}`],
    ['shipped woff2', facesLoaded, 'declared faces report loaded, not error'],
    ['block stacks', stackCarries, 'every measured block names Fira Code'],
    ['column', columnOk, `both views fit ${COLUMNS} characters`],
    ['control (system)', controlDiffers, `system stack measures ${control.face.advance}`],
  ];
  say(checks.map(([name, ok, why]) => `${name.padEnd(18)} ${ok ? 'PASS' : 'FAIL'} (${why})`));
  if (checks.some(([, ok]) => !ok)) process.exitCode = 1;
}

await main();
