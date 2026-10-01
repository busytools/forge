import { readFileSync } from 'node:fs';

import { render } from 'svelte/server';
import { describe, expect, it } from 'vitest';

import { homeWire } from '../dev/fixture.data';
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
    onMessage: refuse,
    onStatus: refuse,
    store: refuse,
    settings: refuse,
    status: refuse,
    close: refuse,
  };
}

const draw = (props: { wire?: HomeWire; slot?: SessionSlot } = {}): string =>
  render(Session, {
    props: { slot: props.slot ?? LEAD, connection: untouched(), wire: props.wire ?? homeWire },
  }).body;

/**
 * The `data-k` of every section the page drew, in the order it drew them.
 *
 * The space matters: one section is named `mcp servers`, and a class that
 * stops at the first word answers a narrower question than the one asked.
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
   * a span sized to a monospace line box, and it is disabled because nothing
   * in this view can close a session yet.
   */
  it('draws the row chip as a disabled button with a name', () => {
    const chip = /<button[^>]*class="x"[^>]*>/.exec(draw())?.[0] ?? '';
    expect(chip, 'the row chip is not a control').not.toBe('');
    expect(chip, 'the chip promised a click it cannot do').toContain('disabled');
    expect(chip, 'the chip carries no name').toContain(
      'aria-label="closing a session is not available yet"',
    );
  });

  /**
   * Both handles are real controls: a rail folded away leaves no edge behind,
   * so the header is the only way back to it.
   */
  it('draws both rail handles as buttons that state what they do', () => {
    const body = draw();
    expect(body).toContain('aria-label="projects"');
    expect(body).toContain('aria-label="inspector"');
    expect(body).toContain('aria-expanded="true"');
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
    expect(foot).toContain('forge v1.0.105');
    expect(foot).toContain('claude v1.0.0');
    expect(foot, 'a newer claude was not offered').toContain('\u{2191} v1.1.0');
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

  it('draws the inspector banner with the project it is showing', () => {
    const body = draw();
    expect(body).toContain('<span class="n ml">proj</span>');
    expect(body).toContain('aria-label="close the inspector"');
  });
});

const sheet = readFileSync(new URL('../assets/web.css', import.meta.url), 'utf8');

/** The last rule body the sheet writes for this exact selector. */
function body(selector: string): string {
  const escaped = selector.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  const found = [...sheet.matchAll(new RegExp(`^\\s*${escaped}\\s*\\{([^}]*)\\}`, 'gm'))];
  return found.at(-1)?.[1] ?? '';
}

/**
 * The rows a `grid-template-rows` value declares: `minmax(0, 1fr) auto` is two.
 * A `repeat()` counts as the single value it is written as, which reads one
 * row short - enough to miss a defect, never enough to fail a sound sheet.
 */
function rows(track: string): number {
  return track
    .replace(/\([^)]*\)/g, 'x')
    .trim()
    .split(/\s+/).length;
}

/** Whether a `grid-row` value reaches the last of `rowCount` rows, counting `-1` as the last. */
function reaches(declared: string | undefined, rowCount: number): boolean {
  if (declared === undefined) return false;
  const [head, tail] = declared.trim().split(/\s*\/\s*/);
  const start = Number(head);
  const end = tail === undefined ? start : Number(tail) < 0 ? rowCount : Number(tail);
  return start === 1 && end >= rowCount;
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
      props: { sleeping: project?.sleeping ?? [], shown },
    }).body;
  };

  it('starts open when the seat the page is showing is behind it', () => {
    expect(fold('slept-2'), 'the fold hid the seat the page is showing').toContain(
      '<details class="sfold" open',
    );
    expect(fold('lead')).not.toContain('<details class="sfold" open');
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

describe('the rail footer as the sheet lays it out', () => {
  /**
   * The box and the tint wrapped a project AND its workers, so four rows
   * looked selected and none of them said which one was open. The mark is on
   * the active row itself instead.
   */
  it('leaves the project row unboxed and untinted, marking the row instead', () => {
    expect(body('.pj.cur'), 'the project box and its tint are back').toBe('');
    expect(body('.pr.on .dot, .wk.on .dot'), 'the active row carries no accent').toContain(
      'var(--accent)',
    );
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
});

describe('the app grid', () => {
  /**
   * The rails are placed by COLUMN, and auto-flow drops a column-only item in
   * the first row. The composer's row then ends under the chat alone and leaves
   * a band of bare page beneath both rails.
   */
  it('runs both rails to the last row the app declares', () => {
    const rowCount = rows(/grid-template-rows:\s*([^;]+)/.exec(body('.app'))?.[1] ?? '1fr');
    for (const side of ['left', 'right']) {
      expect(
        reaches(/grid-row:\s*([^;]+)/.exec(body(`.app .rail.${side}`))?.[1], rowCount),
        `the ${side} rail stops short of the app's last row`,
      ).toBe(true);
    }
  });
});
