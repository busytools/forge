import { readFileSync } from 'node:fs';

import { render } from 'svelte/server';
import { describe, expect, it } from 'vitest';

import { homeWire } from '../dev/fixture.data';
import { PROTOCOL_VERSION } from '../protocol';
import type { Connection } from '../socket';
import type { AgentRow } from '../wire/home';
import type { SessionSlot } from '../wire/types';
import { closeSeat, forgetClosed } from './close';
import Rail from './Rail.svelte';

/**
 * The rail's own surface, and the rules it draws by.
 *
 * **Where each check can look, and why.** A gap between two figures in a row is
 * a question about TEXT, so it is asserted against the rendered row, which is
 * the artifact the defect is in. Which way a summary aligns its items is a
 * DECLARATION, and jsdom performs no layout, so no rendered assertion can see
 * it: that one is read off the sheet by name. Neither claims anything about
 * what a browser paints - the by-width measurement in the pull request does.
 */

const sheet = readFileSync(new URL('../assets/web.css', import.meta.url), 'utf8');

const LEAD: SessionSlot = { org: 'TestOrg', project: 'proj', label: 'lead' };

/**
 * A connection this sheet never reaches.
 *
 * A row's close chip only acts when it is pressed, and every assertion here
 * is over rendered markup - so this satisfies the prop and throws on any
 * use, which would mean the render had started acting rather than drawing.
 */
function untouched(): Connection {
  const refuse = (): never => {
    throw new Error('the render reached the connection');
  };
  return {
    subscribe: refuse,
    unsubscribe: refuse,
    refresh: refuse,
    dispatch: refuse,
    more: refuse,
    devices: refuse,
    frame: refuse,
    onBrowserAsk: refuse,
    browserRole: refuse,
    onBrowserRole: refuse,
    takeBrowserRole: refuse,
    onMessage: refuse,
    onStatus: refuse,
    store: refuse,
    settings: refuse,
    skew: refuse,
    // The footer reads the protocol pair as it RENDERS, so this one answers:
    // refusing it would be refusing the page, not the connection.
    serverProtocol: () => PROTOCOL_VERSION,
    status: refuse,
    close: refuse,
  };
}

const footer = render(Rail, {
  props: {
    // The fixture's own version stands in a placeholder that renders
    // escaped, so this reads the build the way the wire does.
    home: { ...homeWire, forge_version_short: '1.0.105' },
    current: LEAD,
    now: Date.now(),
    connection: untouched(),
    onclose: () => undefined,
  },
}).body;

/** One rule's declarations, from its selector to the brace that closes it. */
function rule(selector: string): string {
  const at = sheet.indexOf(`${selector} {`);
  expect(at, `${selector} is in the sheet`).toBeGreaterThan(-1);
  const end = sheet.indexOf('}', at);
  return sheet.slice(at, end);
}

describe('the rail footer', () => {
  it('states the server and client protocol pair above the CLI it binds', () => {
    // **The client and a server that disagrees on the protocol refuse each
    // other** (`socket.ts` checks the greeting), so both sides read here as a
    // pair rather than only inside a refusal's own words.
    expect(footer, 'the server build').toContain('v1.0.105');
    expect(footer.split(`socket v${PROTOCOL_VERSION}`).length - 1, 'both sides state it').toBe(2);
    expect(footer.indexOf('server'), 'the pair leads the versions').toBeLessThan(
      footer.indexOf('claude v'),
    );
    expect(footer, 'the build must elide so the socket keeps its lane').toContain('class="vt"');
  });

  it('separates the installed version from the one it moves to', () => {
    // The fixture answers 1.0.0 installed against 1.1.0 published, so the row
    // draws both halves and the separator between them is the whole subject.
    // Read as TEXT rather than as markup: the arrow and the version are two
    // nodes with nothing between them, which is what a reader saw run together.
    const row = footer.slice(footer.indexOf('claude v'));
    const words = row
      .slice(0, row.indexOf('</div>'))
      .replace(/<!--.*?-->/g, '')
      .replace(/<[^>]*>/g, '');
    expect(words, 'the row reads as two figures, not as one').toContain(
      'claude v1.0.0 \u{2192} v1.1.0',
    );
  });

  /**
   * **The row is a transition, and the notice is the half that carries the
   * colour.** The arrow opens the notice span rather than standing between the
   * two versions as a mark of its own: a side arrow is what says the pair is
   * one version moving to another, where the up arrow it replaced read as a
   * badge on the second one. The installed half stays `--muted`; the colour is
   * what says which half is the notice.
   *
   * Read as markup, because whether the arrow sits inside the coloured span is
   * a question about the tree, and no text assertion can see it.
   */
  it('opens the notice with the side arrow, and colours that half alone', () => {
    expect(footer, 'the arrow is no longer part of the notice').toContain(
      '<span class="up">\u{2192} v1.1.0</span>',
    );
    expect(rule('.rfoot .vers .up'), 'the notice lost its colour').toContain('var(--warn)');
    expect(rule('.rfoot .vers .v'), 'the installed half took the notice colour').not.toContain(
      'var(--warn)',
    );
  });
});

describe('the asleep heading', () => {
  it('centres its chevron on the row rather than on the text baseline', () => {
    // `baseline` rides the icon on the label's baseline, which drew it about
    // 4px above the row's centre - the one fold on the page that did.
    const gfold = rule('details.gfold > summary');
    expect(gfold).toContain('align-items: center');
    expect(gfold, 'and not the baseline it was drawn on').not.toContain('align-items: baseline');
  });
});

describe('a project row', () => {
  /**
   * The reference the home's rows are held to: a project's row links whether
   * or not anything is behind it, because its page starts the lead - so a
   * SLEEPING project keeps its link, and only the close chip gives way to the
   * age. A worker's sleeping row is the one that is text.
   */
  it('links a sleeping project, whose page starts the lead', () => {
    const template = homeWire.agents[0];
    if (template === undefined) throw new Error('the fixture holds no agent');
    const drawn = render(Rail, {
      props: {
        home: { ...homeWire, agents: [{ ...template, lifecycle: 'Sleeping' }] },
        current: LEAD,
        now: 0,
        connection: untouched(),
        onclose: () => undefined,
      },
    }).body;

    expect(drawn, 'a sleeping project lost its link').toContain(
      'href="/session/TestOrg/proj/lead"',
    );
  });
});

describe('a closed seat', () => {
  /** A connection whose close goes; every other reach is the untouched one. */
  function closes(): Connection {
    return { ...untouched(), dispatch: () => null };
  }

  /**
   * The gap #1712 named: the roster is a read that lands seconds after the
   * click. The row now reads asleep AT ONCE - folded out of the working
   * section on the click - and its dot settles until the roster catches up.
   */
  it('lands a closed row asleep at once, settling until the roster catches up', () => {
    const template = homeWire.agents[0];
    if (template === undefined) throw new Error('the fixture holds no agent');
    const lead: AgentRow = { ...template, lifecycle: 'Running' };
    const worker: AgentRow = { ...template, slot: { ...template.slot, label: 'w1' }, label: 'w1' };
    const wire = { ...homeWire, agents: [lead, worker] };
    const opened = (): string =>
      render(Rail, {
        props: {
          home: wire,
          current: LEAD,
          now: 0,
          connection: closes(),
          onclose: () => undefined,
        },
      }).body;

    try {
      expect(opened(), 'a row nobody closed was settling').not.toContain('settling');

      expect(closeSeat(closes(), wire, worker.slot, LEAD, 0), 'the close never went').toBe(true);
      const drawn = opened();
      expect(drawn, "the closed worker's row did not fold asleep").toContain('1 asleep');
      expect(drawn, 'the settling dot is not on the closed row').toContain(
        '<span class="dot off settling"></span>',
      );
      expect(drawn, 'the settling row did not fold with the closed worker').toContain(
        '<span class="nm">w1</span>',
      );

      // The roster catching up is the seat ARRIVING asleep, still named
      // (a worker's label persists that way), which is when the pulse goes.
      forgetClosed({ ...wire, agents: [lead, { ...worker, lifecycle: 'Sleeping' }] });
      expect(opened(), 'the pulse outlived the roster catching up').not.toContain('settling');

      // The lead's own path: without the mark reaching its row, the project
      // sits under an "asleep" heading drawn as still live. Anchored on the
      // lead's own row, because the fold's worker dot would answer for it.
      expect(
        closeSeat(closes(), wire, lead.slot, worker.slot, 0),
        'the lead close never went',
      ).toBe(true);
      const withLead = opened();
      const leadRow = withLead.slice(withLead.indexOf('class="pr'), withLead.indexOf('class="wk'));
      expect(leadRow, 'the closed lead kept a live dot').toContain(
        '<span class="dot off settling"></span>',
      );
      // The GROUP heading, not the word `asleep`: the sleeper fold's own
      // "1 asleep" would satisfy a looser assertion while the project sat in
      // the working group.
      expect(withLead, "the closed lead's project stayed out of asleep").toContain(
        '<span class="gh">asleep</span>',
      );
    } finally {
      forgetClosed({ ...homeWire, agents: [] });
    }
  });
});

describe('the sheet carries the settling pulse and the waking sweep', () => {
  /**
   * Both marks are drawn AND animated by the sheet, so a rename or an
   * "unused-looking" cleanup would take them away with every other test
   * green. And each reduced-motion arm must sit AFTER its own base: a media
   * query adds no specificity, so an arm above the rule it stills loses to
   * it by source order - which is how the sweep kept running once.
   */
  it('declares each mark, and stills each after its own base', () => {
    expect(rule('.dot.off.settling'), 'the settling pulse went').toContain('animation: pulse');
    expect(rule('.shimmer'), 'the waking sweep went').toContain('animation: shimmer 1.4s');

    const settlingBase = sheet.indexOf('.dot.off.settling {');
    const settlingArm = sheet.indexOf('.dot.off.settling { animation: none');
    expect(settlingArm, 'the settling pulse has no reduced-motion arm').toBeGreaterThan(-1);
    expect(
      settlingArm,
      'the settling arm sits above its base and loses to it by source order',
    ).toBeGreaterThan(settlingBase);

    const sweepBase = sheet.indexOf('.shimmer {');
    const sweepArm = sheet.indexOf('.shimmer { animation: none');
    expect(sweepArm, 'the waking sweep has no reduced-motion arm').toBeGreaterThan(-1);
    expect(
      sweepArm,
      'the sweep arm sits above its base and loses to it by source order',
    ).toBeGreaterThan(sweepBase);
  });
});

describe("a worker's failure", () => {
  /**
   * **The diagnostic draws under the worker's own row**, the way the terminal
   * draws it: a worker's spawn error aggregated onto the project's line put it
   * under the lead's row, which read as the lead having failed while the
   * worker's own row said nothing.
   */
  it("draws the reason under the worker's row, not the project's", () => {
    const template = homeWire.agents[0];
    if (template === undefined) throw new Error('the fixture holds no agent');
    const reason = 'transport closed before initialize';
    const drawn = render(Rail, {
      props: {
        home: {
          ...homeWire,
          agents: [
            { ...template, lifecycle: 'Running', pending: null },
            {
              ...template,
              slot: { ...template.slot, label: 'client-dev' },
              label: 'client-dev',
              lifecycle: 'Failed',
              pending: null,
              reason,
            },
          ],
        },
        current: LEAD,
        now: 0,
        connection: untouched(),
        onclose: () => undefined,
      },
    }).body;

    // Anchored on the failed worker's OWN row rather than the document's
    // first worker row: a fixture carrying another project ahead of this one
    // would satisfy a looser anchor without the row under test being read.
    const workerAt = drawn.indexOf('href="/session/TestOrg/proj/client-dev"');
    expect(workerAt, 'the failed worker has no row').toBeGreaterThan(-1);
    expect(
      drawn.slice(0, workerAt),
      "a worker's failure drew under the project's line",
    ).not.toContain(reason);
    expect(drawn.slice(workerAt), "the worker's own row lost its dim reason line").toContain(
      `<div class="why">${reason}</div>`,
    );
    expect(drawn.split(reason).length - 1, 'the diagnostic drew more than once').toBe(1);
  });
});

describe('a row that is not a way in', () => {
  /**
   * The chrome is the other half of the claim: a row that says "information"
   * in its markup still reads as a link if it points, glows under the pointer
   * and carries the chevron. Keyed on the way-in class rather than on the
   * `asleep` mark, because a dormant lead is asleep and still a link. The
   * rail's fold is scoped to div.wk so its summary - drawn as a worker row,
   * and still the fold's control - keeps its own.
   */
  it('draws no pointer, hover ground or chevron', () => {
    expect(rule('.row.unopenable'), 'the row still points like a link').toContain(
      'cursor: default',
    );
    expect(rule('.row.unopenable::after'), 'the chevron still says it goes somewhere').toContain(
      'display: none',
    );
    expect(rule('.row.unopenable:hover'), 'the row still glows under the pointer').toContain(
      'background: none',
    );
    expect(rule('details.sfold div.wk'), 'the fold row still points like a link').toContain(
      'cursor: default',
    );
  });
});
