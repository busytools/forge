// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

import { homeWire } from '../dev/fixture.data';
import type { Connection } from '../socket';
import type { SessionSlot } from '../wire/types';
import Palette from './Palette.svelte';

const LEAD: SessionSlot = { org: 'TestOrg', project: 'proj', label: 'lead' };

let app: Record<string, unknown> | null = null;

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  document.body.innerHTML = '';
  history.replaceState(null, '', '/');
});

function stub() {
  const dispatched: unknown[] = [];
  const connection = {
    dispatch: (message: unknown) => {
      dispatched.push(message);
      return null;
    },
  } as unknown as Connection;
  return { connection, dispatched };
}

function draw(
  over: {
    connection?: Connection;
    sessionId?: string | null;
    onclose?: () => void;
    onpeek?: (() => void) | null;
  } = {},
) {
  const closed = vi.fn();
  const peeked = vi.fn();
  app = mount(Palette, {
    target: document.body,
    props: {
      open: true,
      wire: homeWire,
      slot: LEAD,
      connection: over.connection ?? stub().connection,
      sessionId: over.sessionId !== undefined ? over.sessionId : 'd4f70669-1f2a-4c88',
      onclose: over.onclose ?? closed,
      onpeek: over.onpeek !== undefined ? over.onpeek : peeked,
    },
  });
  flushSync();
  return { closed, peeked };
}

const input = () =>
  document
    .querySelector<HTMLInputElement>('#pal-rows')
    ?.closest('.panel')
    ?.querySelector<HTMLInputElement>('input') ?? null;
const rows = () => [...document.querySelectorAll<HTMLButtonElement>('.it')];
const selected = () => document.querySelector<HTMLElement>('.it.sel')?.textContent ?? '';

function type(text: string): void {
  const field = input();
  if (field === null) throw new Error('the palette drew no input');
  field.value = text;
  field.dispatchEvent(new Event('input', { bubbles: true }));
  flushSync();
}

function press(key: string): void {
  input()?.dispatchEvent(new KeyboardEvent('keydown', { key, bubbles: true }));
  flushSync();
}

describe('the command palette', () => {
  /**
   * **The cursor starts on the current project's lead**, so Cmd+K then Enter
   * lands there - and Enter on it goes to that seat's URL.
   */
  it('starts on the lead and Enter opens it', () => {
    const { closed } = draw();
    expect(selected(), 'the walk did not start on the lead').toContain('lead');
    expect(selected(), 'the row says it is the one being shown').toContain('current');

    press('Enter');
    expect(location.pathname, 'Enter did not open the seat').toBe('/session/TestOrg/proj/lead');
    expect(closed, 'and the palette stayed up').toHaveBeenCalled();
  });

  /** Typing filters across every group. */
  it('filters across the groups as it is typed into', () => {
    draw();
    const before = rows().length;
    expect(before, 'the whole list drew nothing').toBeGreaterThan(4);

    type('compact');
    expect(
      rows().map((row) => row.textContent ?? ''),
      'the filter did not narrow',
    ).toEqual([expect.stringContaining('/compact')]);
  });

  /**
   * **A command row sends exactly what the composer sends** for that command,
   * id included, so the CLI's own lifecycle frames carry back.
   */
  it("sends a command row as the composer's own prompt", () => {
    const { connection, dispatched } = stub();
    draw({ connection });
    type('usage');
    press('Enter');

    expect(dispatched, 'Enter sent nothing').toHaveLength(1);
    const sent = dispatched[0] as {
      prompt_under?: { key: SessionSlot; text: string; source: string; uuid: string };
    };
    expect(sent.prompt_under?.key, 'the prompt did not go to the shown seat').toEqual(LEAD);
    expect(sent.prompt_under?.text, 'the command text was not what the composer sends').toBe(
      '/usage',
    );
    expect(typeof sent.prompt_under?.uuid, 'the prompt went without its id').toBe('string');
  });

  /** The doings run their own things: peek opens the rail, Escape closes. */
  it('runs the doings and closes on Escape', async () => {
    const first = draw();
    press('Escape');
    expect(first.closed, 'Escape did not close').toHaveBeenCalled();

    if (app !== null) await unmount(app);
    app = null;
    document.body.innerHTML = '';
    const second = draw();
    type('peek at the fleet');
    press('Enter');
    expect(second.peeked, 'the peek doing did not open the rail').toHaveBeenCalled();
  });

  /**
   * **A columned rail offers no peek**: the doing would have nothing to
   * open, and a row that silently does nothing is worse than no row.
   */
  it('drops the peek doing while the rail is the column', () => {
    draw({ onpeek: null });
    type('peek at the fleet');
    expect(rows(), 'a peek row survived with nothing to open').toHaveLength(0);
  });

  /** And a seat with no occupant offers no copy, for the same reason. */
  it('drops the copy doing where there is no occupant', () => {
    draw({ sessionId: null });
    type('copy the session id');
    expect(rows(), 'a copy row survived with nothing to copy').toHaveLength(0);
  });

  /** A filter that matches nothing says so rather than drawing an empty box. */
  it('draws the empty state when nothing matches', () => {
    draw();
    type('zzzz');
    expect(rows(), 'rows survived a filter matching nothing').toHaveLength(0);
    expect(document.querySelector('.none')?.textContent, 'no empty copy').toContain(
      'nothing matches',
    );
  });

  /**
   * **A command reads once**: the label carries its own slash, so the row's
   * mark is the same run glyph every action wears rather than a second
   * slash ahead of it.
   */
  it('marks a command row with the run glyph, not a second slash', () => {
    draw();
    type('compact');
    const row = rows()[0];
    expect(row?.querySelector('.gl')?.textContent, 'the row drew a second slash').toBe('›');
    expect(row?.querySelector('.nm')?.textContent?.trim()).toBe('/compact');
  });
});
