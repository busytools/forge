#!/usr/bin/env node
// Measure the takeover bar at the phone widths and refuse an off-screen control.
//
// **The bar's overflow is a geometry fact, and jsdom has no layout**, so this
// runs against a real built app: build the bundle with `--features
// look-driver`, launch it with `FORGE_LOOK_SOCKET=<path>`, park a browser
// hand-off whose takeover is open, and run this script. Every control must sit
// wholly inside the window, and a hand-off's Done and Not now must be there:
// they are the two answers a person could not reach at 393 points.
//
// Usage: look-bar-geometry.mjs [socket] [width...]
import net from 'node:net';

const socket = process.argv[2] ?? process.env.FORGE_LOOK_SOCKET ?? '/tmp/forge-look.sock';
const widths = process.argv
  .slice(3)
  .map(Number)
  .filter((width) => Number.isFinite(width));
for (const width of widths.length > 0 ? widths : [393, 430]) {
  const inner = await ask({ cmd: 'size', w: width, h: 852 });
  if (inner.error) fail(`the window could not take ${width} points: ${inner.error}`);
  const measured = await ask({
    cmd: 'eval',
    js: `JSON.stringify((() => {
      const bar = document.querySelector('.takeover-bar');
      if (bar === null) return { error: 'no takeover bar: open the takeover first' };
      const box = (element) => {
        const rect = element.getBoundingClientRect();
        return { x: rect.x, right: rect.right, y: rect.y, width: rect.width, height: rect.height };
      };
      return {
        inner: [innerWidth, innerHeight],
        coarse: matchMedia('(pointer: coarse)').matches,
        page: document.documentElement.scrollWidth,
        bar: box(bar),
        buttons: [...bar.querySelectorAll('.tb-btn')].map((button) => ({
          text: button.textContent.trim().replace(/\\s+/g, ' '),
          ...box(button),
        })),
      };
    })())`,
  });
  if (measured.error) fail(`${width}: ${measured.error}`);
  const problems = [];
  if (Math.abs(measured.inner[0] - width) > 1) {
    problems.push(`the window is ${measured.inner[0]} points wide, not ${width}`);
  }
  if (measured.page > measured.inner[0]) {
    problems.push(`the page is ${measured.page} wide in a ${measured.inner[0]}-point window`);
  }
  for (const button of measured.buttons) {
    if (button.x < -0.5 || button.right > measured.inner[0] + 0.5) {
      problems.push(`"${button.text}" sits at ${button.x.toFixed(1)}..${button.right.toFixed(1)}`);
    }
    if (measured.coarse && (button.width < 44 || button.height < 44)) {
      problems.push(`"${button.text}" is ${button.width}x${button.height}, under the 44px target`);
    }
  }
  const texts = measured.buttons.map((button) => button.text);
  for (const answer of ['Done', 'Not now']) {
    if (!texts.includes(answer)) problems.push(`no "${answer}": the hand-off is not parked`);
  }
  const report = {
    width,
    inner: measured.inner,
    coarse: measured.coarse,
    page: measured.page,
    bar: measured.bar,
    buttons: measured.buttons,
  };
  if (problems.length > 0) {
    console.error(JSON.stringify({ ...report, problems }, null, 2));
    process.exit(1);
  }
  console.log(JSON.stringify(report));
}

function ask(request) {
  return new Promise((resolve, reject) => {
    const client = net.createConnection(socket);
    let buffer = '';
    const timer = setTimeout(() => {
      client.destroy();
      reject(new Error(`the look driver did not answer ${request.cmd} within ten seconds`));
    }, 10_000);
    client.on('data', (chunk) => {
      buffer += chunk.toString();
      const line = buffer.indexOf('\n');
      if (line < 0) return;
      clearTimeout(timer);
      client.end();
      try {
        resolve(JSON.parse(buffer.slice(0, line)));
      } catch (why) {
        reject(new Error(`the look driver answered neither JSON nor a line: ${why.message}`));
      }
    });
    client.on('error', (why) => {
      clearTimeout(timer);
      reject(why);
    });
    client.on('connect', () => client.write(`${JSON.stringify(request)}\n`));
  });
}

function fail(why) {
  console.error(why);
  process.exit(1);
}
