import { readFileSync } from 'node:fs';

import { render } from 'svelte/server';
import { describe, expect, it } from 'vitest';

import { homeWire } from '../dev/fixture.data';
import { PROTOCOL_VERSION } from '../protocol';
import type { Connection } from '../socket';
import type { AgentRow, HomeWire } from '../wire/home';
import type { SessionSlot } from '../wire/types';
import Session from './Session.svelte';
import SessionId from './SessionId.svelte';
import SleeperFold from './SleeperFold.svelte';
import { railGroups } from './view';

const LEAD: SessionSlot = { org: 'TestOrg', project: 'proj', label: 'lead' };

/**
 * A connection the server render never reaches.
 *
 * The page subscribes in an effect, and `render` from `svelte/server` runs no
 * effects - so this is here to satisfy the prop. Every method throws rather
 * than answering quietly, so a render that did start reaching it fails loudly
 * instead of drawing a page built on a connection that is not there.
 */
function untouched(): Connection {
  const refuse = (): never => {
    throw new Error('the server render reached the connection');
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
    // The rail's footer reads the protocol pair as it RENDERS, so this one
    // answers: refusing it would be refusing the page, not the socket.
    serverProtocol: () => PROTOCOL_VERSION,
    status: refuse,
    close: refuse,
  };
}

const draw = (
  props: { wire?: HomeWire; slot?: SessionSlot; notice?: string | null } = {},
): string =>
  render(Session, {
    props: {
      slot: props.slot ?? LEAD,
      connection: untouched(),
      wire: props.wire ?? homeWire,
      notice: props.notice ?? null,
    },
  }).body;

/**
 * The `data-k` of every section the page drew, in the order it drew them.
 *
 * A no-record server render draws none of them, which is the case below: the
 * `data-k` names are the inspector's, and the page that draws the record's
 * rows is reached over a socket.
 */
function sections(body: string): string[] {
  return [...body.matchAll(/data-k="sec-([a-z ]+)"/g)].map((match) => match[1] ?? '');
}

/** The fixture's home with an account chipped for the seat's project. */
function withAccount(): HomeWire {
  const project = homeWire.projects[0];
  const account = homeWire.accounts.loading[0];
  if (project === undefined || account === undefined) {
    throw new Error('the fixture holds no project or no account');
  }
  return {
    ...homeWire,
    accounts: {
      ...homeWire.accounts,
      loading: [{ ...account, state: 'ready' }],
      usage: [
        {
          display_name: 'Acct',
          snapshot: {
            source: 'OpenRouterKey',
            spend: { daily: 1.5, weekly: 10, monthly: 42, limit: 50 },
            balance: 12.25,
          },
        },
      ],
    },
    projects: [{ ...project, chip: { account_name: 'Acct', state: 'ready' } }],
  };
}

/** The rail's footer alone, up to the column that follows it. */
function footerOf(body: string): string {
  const from = body.indexOf('class="rfoot"');
  if (from < 0) throw new Error('the page drew no rail footer');
  return body.slice(from, body.indexOf('<main', from));
}

describe('the session shell as it draws', () => {
  /**
   * The server's own rule: a section with nothing behind it is not drawn, and
   * it is why a fresh seat's page looks short. This render has no record at
   * all, which is the same shape as a seat with nothing to say.
   */
  it('draws no section before the seat is read', () => {
    expect(sections(draw())).toEqual([]);
  });

  it('draws no header facts before the seat is read', () => {
    const body = draw();
    expect(body, 'a fact nothing has reported was drawn').not.toContain('class="fk"');
  });

  /**
   * The page-level property is that the rail is there at all and points at the
   * seat: the grouping, the fleet count and the reason line are the view's, and
   * `view.test.ts` pins them where they are decided.
   */
  it('draws the projects rail with a row for the seat', () => {
    const body = draw();
    expect(body).toContain('needs you');
    expect(body).toContain('href="/session/TestOrg/proj/lead"');
  });

  /**
   * The close chip is a control with an accessible name rather than a glyph in
   * a span sized to a monospace line box, and the name says what it closes -
   * the row it is drawn on, which is what decides the command it sends
   * (`session/close.test.ts`).
   */
  it('draws the row chip as a button naming what it closes', () => {
    const chip = /<button[^>]*class="x"[^>]*>/.exec(draw())?.[0] ?? '';
    expect(chip, 'the row chip is not a control').not.toBe('');
    expect(chip, 'the chip takes a click it would not answer').not.toContain('disabled');
    expect(chip, 'the chip carries no name').toContain('aria-label="close proj"');
    expect(chip, 'the chip does not say what it closes').toContain('title="close proj"');
  });

  /**
   * **The chip is the one projects control**: the brand's mark as its face,
   * the seats that want a person on its own label, and no inspector handle
   * beside it - that died with the inspector.
   */
  it('draws the projects chip as a real link stating who waits', () => {
    const body = draw();
    expect(body, 'a real anchor').toContain('<a class="needchip"');
    expect(body, 'the count in its label').toContain('aria-label="1 seat needs you"');
    expect(body, 'the mark as its face').toContain('ch-mk');
    expect(body, 'and the needs shape beside the count').toContain('dot needs ch-tri');
    expect(body).not.toContain('aria-label="inspector"');
  });

  /**
   * **The chip's click is smart**: exactly one waiting seat goes straight
   * there, and the band leads with it - the seat's own name behind.
   */
  it("sends the chip's one waiting seat straight to it", () => {
    const body = draw();
    expect(body, 'the single waiting seat').toContain('href="/session/TestOrg/proj/lead"');
    // The band alone, so the rail's own rows cannot answer for it.
    const band = body.slice(body.indexOf('class="sess"'));
    expect(band.indexOf('class="needchip"'), 'the band leads with the chip').toBeLessThan(
      band.indexOf('class="nm"'),
    );
  });

  /** Everything but a lone clean waiter goes to the home page to pick. */
  it('sends a several, a failed and an empty set to the home', () => {
    const agent = (label: string, over: Partial<AgentRow> = {}): AgentRow => ({
      slot: { org: 'TestOrg', project: 'proj', label },
      label,
      lifecycle: 'Idle',
      has_background_work: false,
      pending: 'permission',
      pending_depth: 1,
      last_activity: null,
      reason: null,
      failed_turn: null,
      work: null,
      ...over,
    });
    const many = draw({ wire: { ...homeWire, agents: [agent('a'), agent('b')] } });
    expect(many, 'several pick at home').toContain('href="/"');
    expect(many, 'counted').toContain('aria-label="2 seats need you"');

    const failed = draw({
      wire: {
        ...homeWire,
        agents: [agent('a', { failed_turn: { secs_since_epoch: 1, nanos_since_epoch: 0 } })],
      },
    });
    expect(failed, 'a failure goes home even alone').toContain('href="/"');
    expect(failed, 'and wears the cross').toContain('needchip bad');

    const calm = draw({ wire: { ...homeWire, agents: [] } });
    expect(calm, 'nothing waits: home from a quiet word').toContain('aria-label="projects"');
    expect(calm, 'calm').toContain('needchip calm');
  });

  it('draws the seat state rather than an empty column when nothing is running', () => {
    const body = draw({
      wire: { ...homeWire, agents: [] },
      slot: { org: 'TestOrg', project: 'proj', label: 'never-started' },
    });
    expect(body).toContain('not running');
    expect(body).toContain('this seat has no session behind it');
  });

  it('draws the seat line only for a seat nothing is running', () => {
    expect(draw(), 'a seat with a session behind it claimed nothing was running').not.toContain(
      'not running',
    );
  });

  /**
   * The footer is the rail's own, below the list rather than inside it: the
   * account and the versions stay where they are while the projects scroll.
   */
  it('draws the account, its five figures and the versions in the rail footer', () => {
    // The fixture's own version stands in a placeholder that renders escaped,
    // so this reads the build the way the wire does.
    const foot = footerOf(draw({ wire: { ...withAccount(), forge_version_short: '1.0.105' } }));
    expect(foot).toContain('Acct');
    for (const figure of ['day', 'week', 'month', 'balance', 'cap']) {
      expect(foot, `the footer dropped the ${figure} row`).toContain(figure);
    }
    expect(foot).toContain('$1.50');
    expect(foot).toContain('$50.00');
    // The pair, both sides stated: the server's build beside the protocol it
    // speaks, and this client's beside its own. A mismatch is then a
    // difference the reader sees rather than a notice they must decode.
    expect(foot).toContain('v1.0.105');
    expect(foot.split(`socket v${PROTOCOL_VERSION}`).length - 1, 'both sides state it').toBe(2);
    expect(foot).toContain('claude v1.0.0');
    expect(foot, 'a newer claude was not offered').toContain('\u{2192} v1.1.0');
  });

  /**
   * The account reads by its NAME and nothing else: the billing word he cut,
   * and the pool's repair word beside it, are both gone from the rail.
   */
  it('names the account with no billing word under it', () => {
    const foot = footerOf(draw({ wire: withAccount() }));
    expect(foot, 'a billing word reached the footer').not.toContain('token');
    expect(foot, 'the account state word reached the footer').not.toContain('ready');
  });

  it('draws no header chip, whose slot the session id takes', () => {
    expect(draw({ wire: withAccount() }), 'the account chip is still in the header').not.toContain(
      'class="acct"',
    );
  });
});

const sheet = readFileSync(new URL('../assets/web.css', import.meta.url), 'utf8');

/** One selector's declarations, whitespace-normalised, or '' where it has none. */
function ruleText(source: string, selector: string): string {
  const escaped = selector.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  const found = [...source.matchAll(new RegExp(`^\\s*${escaped}\\s*\\{([^}]*)\\}`, 'gm'))];
  return (found.at(-1)?.[1] ?? '').replace(/\s+/g, ' ').trim();
}

describe('the header brand as both sheets draw it', () => {
  /**
   * **The drawing is the visual truth, so a rule edited in one sheet alone is
   * one surface described two ways.** The chip's pill and the summoned rail's
   * park-and-slide are exactly the kind of numbers a later pass tweaks in one
   * file - so they are read rule for rule, whitespace-normalised, the way the
   * row rules are.
   */
  it('mirrors the chip and summoned-rail rules in both sheets', () => {
    const page = readFileSync(
      new URL('../../../docs/book/src/ui/client/web-session.html', import.meta.url),
      'utf8',
    );
    const book = /<style>([\s\S]*?)<\/style>/.exec(page)?.[1] ?? '';
    for (const selector of [
      '.needchip',
      '.needchip.calm',
      '.app .rail.left',
      '.app.rail-open .rail.left',
      '.app.rail-static .rail.left',
      '.pal .grp',
    ]) {
      const app = ruleText(sheet, selector);
      // The denominator: a scan that reaches no rule reports both sheets
      // agreeing on an empty string.
      expect(app, `web.css spells ${selector}`).not.toBe('');
      expect(ruleText(book, selector), `${selector} diverges between the sheets`).toBe(app);
    }
  });
});

/** The last rule body the sheet writes for this exact selector. */
function body(selector: string): string {
  const escaped = selector.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  const found = [...sheet.matchAll(new RegExp(`^\\s*${escaped}\\s*\\{([^}]*)\\}`, 'gm'))];
  return found.at(-1)?.[1] ?? '';
}

/**
 * Every rule the sheet writes whose selector marks a shown row's mark.
 *
 * **The whole set, not one selector's text.** The mark is a declaration
 * somewhere in this set, so a guard pinned to a single rule stays green while a
 * second rule reintroduces the defect - which is exactly what a new selection
 * rule is. Read from selectors written on one line, which is how this sheet
 * writes every one of them; a rule wrapped across lines would escape the scan,
 * so wrap none.
 */
function shownMarkRules(): { selector: string; declares: string }[] {
  const rules: { selector: string; declares: string }[] = [];
  for (const match of sheet.matchAll(/^([^\n{}]*\.on[^\n{}]*)\{([^}]*)\}/gm)) {
    const selector = (match[1] ?? '').trim();
    if (!selector.includes('.dot')) continue;
    rules.push({ selector, declares: match[2] ?? '' });
  }
  return rules;
}

/** A row for the fixture's lead, in the state named. */
function row(label: string, lifecycle: AgentRow['lifecycle']): AgentRow {
  const first = homeWire.agents[0];
  if (first === undefined) throw new Error('the fixture holds no agent');
  return { ...first, slot: { ...first.slot, label }, label, lifecycle, pending: null };
}

describe('the rail folds', () => {
  /**
   * The asleep section folds, and the heading carries what it hides: a fold
   * that read as an empty section would be worse than the rows it replaced.
   */
  it('folds the asleep heading, counting the rows behind it', () => {
    const body = draw({ wire: { ...homeWire, agents: [row('lead', 'Sleeping')] } });
    expect(body).toContain('class="gfold"');
    expect(body).toContain('<span class="gh">asleep</span>');
    expect(body, 'the fold does not say how much it hides').toContain('<span class="cn">1</span>');
  });

  /**
   * A live project keeps its awake workers on the rows and folds the sleeping
   * ones behind one of their own, which counts them.
   */
  it("folds a project's sleeping workers behind one counted row", () => {
    const body = draw({
      wire: {
        ...homeWire,
        agents: [
          row('lead', 'Running'),
          row('awake', 'Running'),
          row('slept-1', 'Sleeping'),
          row('slept-2', 'Sleeping'),
        ],
      },
    });
    expect(body).toContain('class="sfold"');
    expect(body, 'the row hiding the sleeping seats does not count them').toContain('2 asleep');
    expect(body, 'an awake worker was folded away').toContain('href="/session/TestOrg/proj/awake"');
  });

  it('draws no fold for a project whose workers are all awake', () => {
    const body = draw({
      wire: { ...homeWire, agents: [row('lead', 'Running'), row('awake', 'Running')] },
    });
    expect(body, 'a project with nothing asleep drew a fold').not.toContain('class="sfold"');
  });
});

describe('the rail row as the sheet lays it out', () => {
  /**
   * A long project name gives way rather than the row: the name elides and the
   * age keeps the row's own right edge. `flex: none` here is the defect #1866
   * filed - a name like `pure-black-github-intellij` then pushes the age off
   * the panel - and dropping `min-width: 0` restores the same floor, because a
   * flex item's min-content width is what refuses to shrink.
   */
  it('makes the name the thing that gives way, keeping the age its edge', () => {
    const rule = body('.pr .nm');
    expect(rule, 'the name takes its whole width and pushes the rest out').toContain(
      'flex: 0 1 auto',
    );
    expect(rule, 'the name cannot shrink below its own text').toContain('min-width: 0');
    expect(rule, 'the name clips rather than showing it was cut').toContain(
      'text-overflow: ellipsis',
    );
  });

  /**
   * The rail's other overflow risk: a spawn failure names a path with no break
   * in it, and the line keeps to the rail's width instead of laying a second
   * scrollbar across it. The terminal truncates this sub-row the same way.
   */
  it('truncates a reason line rather than letting it run', () => {
    const rule = body('.pj .why');
    expect(rule, 'a long reason lays a scrollbar across the rail').toContain(
      'text-overflow: ellipsis',
    );
    expect(rule, 'the line can still overrun the panel while clipped').toContain(
      'overflow: hidden',
    );
  });

  /**
   * **The finger's target is the one place the ellipsis is easy to lose.** The
   * coarse block gives a row's link its 44px box, and an `inline-flex` anchor
   * is an atomic inline: `text-overflow` does not paint across one, so the
   * phone clipped a long name with no ellipsis at all. The height rides the
   * line instead, which is what the sheet already does for an artifact link.
   */
  it('keeps the ellipsis on a finger-driven screen, where the target is a box', () => {
    const rule = body('.pr .nm a, .wk .nm a, .row .name a');
    expect(rule, 'the coarse rule draws a name it cannot ellipsise').toContain('text-overflow');
    // Block and not an atomic inline: measured in WebKit on a shrunk row, an
    // inline-block anchor sized to its text and ran past the panel with no
    // ellipsis, where a block one took the parent's width and painted it.
    expect(rule, 'the anchor is atomic again, and the ellipsis goes with it').toContain(
      'display: block',
    );
    expect(rule, 'an atomic inline would ellipsise nothing').not.toContain('inline-flex');
    expect(rule, 'the finger lost the 44px target').toContain('min-height: 44px');
  });
});

describe('the active row', () => {
  it('marks the seat the page is showing, on that row', () => {
    expect(draw(), 'the seat the page is showing is not marked').toContain('class="pr on"');
  });

  it('marks a worker row when the page is showing a worker', () => {
    const body = draw({
      slot: { ...LEAD, label: 'w1' },
      wire: { ...homeWire, agents: [...homeWire.agents, row('w1', 'Running')] },
    });
    expect(body).toContain('class="wk on"');
    expect(body, 'the project lit its own row as well').not.toContain('class="pr on"');
  });
});

describe("a project's sleeping seats", () => {
  /** The fold as the rail hands it the rows, taken from the real grouping. */
  const fold = (shown: string | null): string => {
    const home: HomeWire = {
      ...homeWire,
      agents: [row('lead', 'Running'), row('slept-1', 'Sleeping'), row('slept-2', 'Sleeping')],
    };
    const project = railGroups(home, LEAD, 0).flatMap((group) => group.projects)[0];
    return render(SleeperFold, {
      props: { sleeping: project?.sleeping ?? [], shown, closing: () => false },
    }).body;
  };

  it('starts open when the seat the page is showing is behind it', () => {
    expect(fold('slept-2'), 'the fold hid the seat the page is showing').toContain(
      '<details class="sfold" open',
    );
    expect(fold('lead')).not.toContain('<details class="sfold" open');
  });

  /**
   * The summary wears what the rows behind it wear, settling included: the
   * fold is display, so the one sign that a seat inside is still shutting down
   * cannot be put away with the rows.
   */
  it('wears the pulse the rows behind it wear, and rests when they do', () => {
    const home: HomeWire = {
      ...homeWire,
      agents: [row('lead', 'Running'), row('slept-1', 'Sleeping')],
    };
    const project = railGroups(home, LEAD, 0).flatMap((group) => group.projects)[0];
    const sleeping = project?.sleeping ?? [];
    expect(sleeping.length, 'the fixture drew no sleeping seat to fold').toBeGreaterThan(0);

    const summary = (closing: () => boolean): string => {
      const body = render(SleeperFold, { props: { sleeping, shown: null, closing } }).body;
      return body.slice(body.indexOf('<summary'), body.indexOf('</summary>'));
    };

    expect(
      summary(() => false),
      'a fold with nothing settling wore the pulse',
    ).toContain('<span class="dot off"></span>');
    expect(
      summary(() => false),
      'a fold with nothing settling wore the pulse',
    ).not.toContain('settling');
    expect(
      summary(() => true),
      'the fold put the pulse away with its rows',
    ).toContain('<span class="dot off settling"></span>');
  });
});

describe('the session id cell', () => {
  const cell = (id: string): string => render(SessionId, { props: { id } }).body;

  /**
   * The terminal's own shape: eight characters on the row, the whole id on
   * the control, because the short form is what a reader compares and the long
   * one is what a person pastes into a resume.
   */
  it('draws the id short, with the whole one under it', () => {
    const body = cell('d4f70669-1f2a-4b3c-9d0e');
    expect(body).toContain('>d4f70669<');
    expect(body, 'the short form is all a reader can reach').toContain(
      'title="d4f70669-1f2a-4b3c-9d0e"',
    );
  });

  it('carries a copy control with a name that says what it copies', () => {
    const body = cell('d4f70669');
    expect(body).toContain('aria-label="copy the whole session id"');
    expect(body, 'the control is not a control').toContain('<button');
    expect(body, 'the control promised a click it cannot do').not.toContain('disabled');
  });
});

describe('the bar fills', () => {
  /**
   * **A fill is a box, or it paints nothing.** A non-replaced inline box takes
   * neither the width a percentage asks for nor a height, so a fill written as
   * an inline span draws an empty track with its own figure beside it - which
   * is what both bars did, the context cell's and the window bars'.
   *
   * One rule for every bar rather than one per surface: the two were the same
   * defect wearing two hats, and a fix that names one while carrying the other
   * is how the second survives the next refactor.
   */
  it('gives every fill a box, from one rule', () => {
    expect(body('.tk > .fl'), 'the fill has no box to paint in').toContain('display: block');
    expect(body('.cm .fl'), 'a rule for one bar alone came back').toBe('');
    expect(body('.bar .fl'), 'a rule for the other bar alone came back').toBe('');
  });
});

describe('the rail footer as the sheet lays it out', () => {
  /**
   * The box and the tint wrapped a project AND its workers, so four rows
   * looked selected and none of them said which one was open. The mark is on
   * the active row itself instead.
   */
  it('leaves the project row unboxed and untinted, marking the row instead', () => {
    expect(body('.pj.cur'), 'the project box and its tint are back').toBe('');
    const marked = shownMarkRules()
      .map((rule) => rule.declares)
      .join(' ');
    expect(marked, 'the active row carries no accent').toContain('var(--accent)');
  });

  /**
   * **A mark's shape is the state; selection may only recolour it.** A running
   * seat draws the loader and an idle one draws a solid disc, so a fill on the
   * shown row's mark makes a working seat read as one at rest. The plausible
   * change this catches is the one that wrote such a fill in the first place:
   * reaching for more prominence on the shown row.
   */
  it('reshapes no mark on the shown row', () => {
    const rules = shownMarkRules();
    expect(rules.length, 'no rule marks the shown row at all').toBeGreaterThan(0);
    for (const { selector, declares } of rules) {
      expect(
        declares,
        `${selector} reshapes the mark, which erases the state the mark carries`,
      ).not.toMatch(
        /(^|[;\s])(background(-color)?|clip-path|border-radius|border-width|width|height|flex):/,
      );
    }
  });

  /**
   * **The arc is the running state, so selection may not flatten it.** A
   * running mark's ring carries one bright side and a faint track, and an
   * asleep one is a plain ring; colouring all four sides alike drew a shown
   * running seat exactly as a shown asleep one, identical under reduced motion
   * where the spin is not there to separate them either. So the shown row
   * brightens the track and states the arc at the accent.
   */
  it("keeps the shown row's arc above the track it raises", () => {
    const rules = shownMarkRules();
    const arcs = rules.filter((rule) => /(^|[;\s])border-top-color:/.test(rule.declares));
    expect(
      arcs.length,
      'the shown row colours every side alike, so a running mark draws as an asleep one',
    ).toBeGreaterThan(0);
    for (const { selector, declares } of arcs) {
      expect(declares, `${selector} darkens the running mark's arc`).toContain(
        'border-top-color: var(--accent)',
      );
    }
  });

  /**
   * **The mark has to move something the state does not already occupy.** An
   * idle dot IS the accent, so a mark that recoloured only the dot would paint
   * nothing on the ordinary state of a shown seat - a seat's mark is cleared
   * when a turn ends, so a seat at rest is idle. The terminal draws selection
   * the same way: the glyph stays the session's state and the NAME takes the
   * accent.
   */
  it('marks the row name, not only a dot the idle state already accents', () => {
    expect(
      body('.pr.on .nm, .wk.on .nm'),
      'the mark recolours only the dot, which an idle seat already carries',
    ).toContain('var(--accent)');
  });

  /**
   * The scrolling goes BEHIND the footer, not with it: the list is the rail's
   * one scroller, and the footer is the sibling that takes the height it needs
   * rather than a share of what is left. A footer that could shrink is one a
   * long list squashes.
   */
  it('keeps the footer out of the rail scroller and unsquashable', () => {
    expect(body('.rail .scroll'), 'the list is no longer the scroller').toContain(
      'overflow-y: auto',
    );
    expect(body('.rfoot'), 'the footer scrolls with the list').not.toContain('overflow-y');
    expect(body('.rfoot'), 'a long list can squash the footer').toContain('flex: none');
  });

  /**
   * **The rail's x axis is pinned shut**, so no row can lay a second scrollbar
   * across the foot of the list: the corner that one makes with the vertical
   * bar is the white block #1866 saw. The rows elide and the reason lines
   * truncate, so this is the belt rather than the fix.
   */
  it('refuses a horizontal scrollbar, whatever a row does', () => {
    expect(
      body('.rail .scroll'),
      'a row that outgrows the panel scrolls the rail sideways',
    ).toContain('overflow-x: hidden');
  });
});

describe('the app grid', () => {
  /**
   * **The default rail is the column, and the other two modes are the
   * summoned overlay.** A column rides the app grid; a hover-mode or closed
   * rail parks off-canvas and slides in over a scrim, so it costs the
   * conversation nothing while it is away.
   */
  it('draws the column by default and the parked rail for the other modes', () => {
    expect(body('.app.rail-static .rail.left'), 'the static rail is the column').toContain(
      'position: static',
    );
    expect(body('.app .rail.left'), 'the summoned rail is fixed').toContain('position: fixed');
    expect(body('.app .rail.left'), 'and parked off-canvas').toContain(
      'translateX(calc(-100% - 24px))',
    );
    expect(body('.app.rail-open .rail.left'), 'sliding in when summoned').toContain(
      'translateX(0)',
    );
    expect(body('.app main.chat'), 'the chat owns the first column').toContain('grid-column: 1');
  });

  /**
   * **The pin says where the rail is next, and the close puts it away.**
   * The server render has no DOM, so it takes the column default; the pin's
   * word is the action rather than the state.
   */
  it('draws the rail as the column, pinned, with its own two controls', () => {
    const body = draw();
    expect(body, 'the default was not the column').toContain('rail-static');
    expect(body, 'the pin floats the rail').toContain('float the rail on hover');
    expect(body, 'the rail grew a close again').not.toContain('close the projects rail');
  });

  /**
   * **A stale read states itself on the chip.** The connection's own line
   * lives in the rail's footer, and the rail may be away or folded - the
   * chip is the page's visible pane element, so the line rides it too
   * rather than vanishing with the rail.
   */
  it('marks the chip while the page shows a stale read', () => {
    const body = draw({ notice: 'the socket dropped; showing the last read' });
    expect(body, 'the stale state vanished with the rail away').toContain('ch-nd');
    // The NAME, not just the title: the cross is decorative and the tone is
    // colour, so the line has to reach the accessible label itself.
    expect(body, 'the stale line is not in the accessible name').toContain(
      'aria-label="1 seat needs you, the socket dropped; showing the last read"',
    );
  });

  /**
   * **The end of a conversation reads like the middle of one.** The last row's
   * own trailing space and the composer's own edge are the whole of the space
   * between them: the column draws no bottom padding, and this row cancels the
   * grid's row gap for itself - `row-gap: 0` on the app measures the same
   * today and would leave any row added later unspaced too.
   */
  it("ends the conversation at the composer, with no gap of the grid's", () => {
    // Every rule the sheet writes for `.composer`, not the last one: the
    // mobile query's comes after the base rule and would answer for it.
    const composer = [...sheet.matchAll(/^\s*\.composer\s*\{([^}]*)\}/gm)].map(
      (hit) => hit[1] ?? '',
    );
    expect(
      composer.some((rule) => rule.includes('margin-top: calc(-1 * var(--ins))')),
      'the grid row gap is back above the composer',
    ).toBe(true);
    expect(body('.conv'), 'the column reserves bottom padding again').toContain(
      'padding: 2px 8px 0 0',
    );
  });
});
