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
   * Both handles are real controls: a rail folded away leaves no edge behind,
   * so the header is the only way back to it.
   */
  it('draws both rail handles as buttons that state what they do', () => {
    const body = draw();
    expect(body).toContain('aria-label="projects"');
    expect(body).toContain('aria-label="inspector"');
    expect(body).toContain('aria-expanded="true"');
  });

  /**
   * **The wordmark in the header is the way home.** A real anchor, so the
   * keyboard reaches it, the URL stays real and the router follows it in place
   * (it is the only way back to the home inside a desktop shell, which has no
   * browser chrome); the hairline is what separates the app's brand from the
   * seat's own, and the band leads with it.
   */
  it('draws the wordmark in the header as the way home', () => {
    const body = draw();
    expect(body, 'a real anchor to the home').toContain('<a class="brand" href="/"');
    expect(body, "the home's own word").toContain('>forge</span>');
    expect(body, 'and the hairline after it').toContain('class="mastsep"');
    expect(body.indexOf('class="brand"'), 'the band leads with it').toBeLessThan(
      body.indexOf('rail-tog'),
    );
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

  it('draws the inspector banner with the project it is showing', () => {
    const body = draw();
    expect(body).toContain('<span class="n ml">proj</span>');
    expect(body).toContain('aria-label="close the inspector"');
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
   * one surface described two ways.** The wordmark's size relative to the
   * session's own name, and the hairline that separates them, are exactly the
   * kind of numbers a later pass tweaks in one file - so they are read rule
   * for rule, whitespace-normalised, the way the row rules are.
   */
  it('mirrors the brand and hairline rules in both sheets', () => {
    const page = readFileSync(
      new URL('../../../docs/book/src/ui/client/web-session.html', import.meta.url),
      'utf8',
    );
    const book = /<style>([\s\S]*?)<\/style>/.exec(page)?.[1] ?? '';
    for (const selector of [
      '.sess .brand',
      '.sess .brand .mark',
      '.sess .brand .word',
      '.sess .mastsep',
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
      props: { sleeping: project?.sleeping ?? [], shown, closing: () => false },
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
