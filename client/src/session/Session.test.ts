import { readFileSync } from 'node:fs';

import { render } from 'svelte/server';
import { describe, expect, it } from 'vitest';

import { homeWire } from '../dev/fixture.data';
import type { Connection } from '../socket';
import type { HomeWire } from '../wire/home';
import type { SessionSlot } from '../wire/types';
import Session from './Session.svelte';

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

  it('draws the account chip for the account the project would bind to', () => {
    const project = homeWire.projects[0];
    const account = homeWire.accounts.loading[0];
    if (project === undefined || account === undefined) {
      throw new Error('the fixture holds no project or no account');
    }
    const body = draw({
      wire: {
        ...homeWire,
        accounts: {
          ...homeWire.accounts,
          loading: [{ ...account, state: 'ready' }],
        },
        projects: [{ ...project, chip: { account_name: 'Acct', state: 'ready' } }],
      },
    });
    expect(body).toContain('class="acct"');
    expect(body).toContain('Acct');
    expect(body).toContain('class="st ok">ready');
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
