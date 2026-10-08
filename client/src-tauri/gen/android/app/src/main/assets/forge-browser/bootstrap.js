'use strict';
// The in-app driver host: runs the vendored @playwright/mcp CLI inside the
// app process under libnode, speaking MCP to the client's Rust side over a
// unix socket and CDP to the app's own WebView over the Kotlin relay.
//
// Why the emulated stdio pair: the desktop client spawns cli.js and speaks
// MCP over its stdio. Android has no second node binary to spawn, so cli.js
// runs in THIS process with process.stdin/process.stdout replaced by a pair
// of streams bridged to the socket. The server sees the exact transport the
// desktop wires.
//
// Measured on the way here (spike, issue #1839), each load-bearing:
//   - libnode reports platform 'android'; playwright-core's registry has no
//     android arm and throws - present as linux.
//   - playwright artifacts default to os.tmpdir()/pw-*; an app has no
//     writable /tmp, and TMPDIR env alone does not take (libuv caches the
//     resolved value) - patch os.tmpdir() itself.
//   - the registry cache is $XDG_CACHE_HOME or homedir()/.cache, neither
//     writable for an app - point both at the app's own directories.
//   - process.exit aborts in libnode's teardown and takes the whole app down
//     (FORTIFY mutex, both Node 18 and 24) - this file never exits.

const fs = require('fs');
const net = require('net');
const os = require('os');
const path = require('path');
const { PassThrough } = require('stream');

function parseArgs(argv) {
  const out = {};
  for (let i = 2; i < argv.length; i++) {
    const a = argv[i];
    if (a === '--mcp') out.mcp = argv[++i];
    else if (a === '--mcp-socket') out.mcpSocket = argv[++i];
    else if (a === '--cdp') out.cdp = argv[++i];
    else if (a === '--cdp-token') out.cdpToken = argv[++i];
    else if (a === '--output') out.output = argv[++i];
  }
  return out;
}

const opts = parseArgs(process.argv);
console.error('[bootstrap] payload up ' + process.version + ' at ' + Date.now());
if (!opts.mcp || !opts.mcpSocket || !opts.cdp || !opts.cdpToken || !opts.output) {
  // **Names the missing field, never the object**: stringifying `opts` would
  // put the relay token in logcat on a lost field.
  const missing = ['mcp', 'mcpSocket', 'cdp', 'cdpToken', 'output'].filter(function (key) {
    return !opts[key];
  });
  console.error('[bootstrap] missing an argument: ' + missing.join(', '));
  throw new Error('the bootstrap was not given its full argv');
}

// **stderr, never stdout**: once the stream pair below is installed,
// `process.stdout` IS the MCP channel - a log line written there is non-JSON
// garbage in the server's transport, which wedges the handshake (measured:
// the first dial connected and the initialize never answered).
function log(line) {
  console.error('[bootstrap] ' + line);
}

// --- the environment fixes the driver needs on Android ---------------------

if (process.platform === 'android') {
  Object.defineProperty(process, 'platform', { value: 'linux' });
}
const baseDir = path.dirname(opts.output);
const tmpDir = path.join(baseDir, 'tmp');
const cacheDir = path.join(baseDir, 'cache');
fs.mkdirSync(tmpDir, { recursive: true });
fs.mkdirSync(cacheDir, { recursive: true });
process.env.TMPDIR = tmpDir;
process.env.TMP = tmpDir;
process.env.TEMP = tmpDir;
process.env.XDG_CACHE_HOME = cacheDir;
process.env.HOME = baseDir;
os.tmpdir = function () { return tmpDir; };

// --- the MCP transport: a socket wearing stdio ----------------------------

const toServer = new PassThrough();
const fromServer = new PassThrough();
Object.defineProperty(process, 'stdin', { value: toServer, configurable: true, enumerable: true, writable: true });
Object.defineProperty(process, 'stdout', { value: fromServer, configurable: true, enumerable: true, writable: true });

// **Dial with retry.** The shell binds its socket just before it asks for
// this process, so the first dial can land a beat early; and a call whose
// start failed is retried against a fresh listener on the same path. The
// server side is one long-lived MCP server, so a redial simply carries
// frames again - nothing is re-handshaken here.
let link = null;
function dial() {
  console.error('[bootstrap] dialing ' + opts.mcpSocket + ' at ' + Date.now());
  link = net.connect({ path: opts.mcpSocket });
  link.on('connect', () => {
    log('mcp socket connected');
  });
  link.on('error', (e) => {
    // The shell is not listening yet (or no longer): the retry below is the
    // answer, and a failure here must never be a crash.
    console.error('[bootstrap] mcp socket error: ' + e.message);
  });
  link.on('close', () => {
    log('mcp socket closed; redialing');
    setTimeout(dial, 1000);
  });
  // **Hand-rolled, not pipe()**: the driver's exit watchdog exits the
  // process on stdin 'end', and process.exit aborts in libnode's teardown -
  // taking the whole app down. A socket that closes must therefore never END
  // the server's stdin; it just stops carrying frames.
  link.on('data', (d) => {
    toServer.write(d);
  });
}
dial();
fromServer.on('data', (d) => {
  if (link && !link.destroyed) link.write(d);
});

// --- the driver -----------------------------------------------------------

const serverArgs = [
  '--cdp-endpoint', opts.cdp,
  '--cdp-header', 'X-Forge-Token: ' + opts.cdpToken,
  '--no-webmcp',
  '--allow-unrestricted-file-access',
  '--output-dir', opts.output,
];
process.argv = ['node', opts.mcp].concat(serverArgs);
log('starting ' + opts.mcp + ' against ' + opts.cdp);
require(opts.mcp);
log('cli.js required');
