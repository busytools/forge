// @vitest-environment jsdom
import { createRawSnippet, flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

import type { AgentRow, HomeWire } from '../wire/home';
import { homeWire } from '../dev/fixture.data';
import GroupFold from './GroupFold.svelte';
import SessionId from './SessionId.svelte';
import SleeperFold from './SleeperFold.svelte';
import { boxed } from './testing/props.svelte';
import { railGroups } from './view';

/**
 * The two controls whose behaviour lives in a HANDLER or an EFFECT, which the
 * server renderer cannot reach: it runs neither, so a suite built on it alone
 * stays green with the click path deleted.
 */

let app: Record<string, unknown> | null = null;

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  document.body.innerHTML = '';
  vi.unstubAllGlobals();
});

/** The clipboard jsdom does not carry, where the test wants one. */
function clipboard(write: (text: string) => Promise<void>): void {
  vi.stubGlobal('navigator', { clipboard: { writeText: write } });
}

/** What the control is owed before it can say anything: a settled write. */
async function settle(): Promise<void> {
  await Promise.resolve();
  flushSync();
}

/** The cell's control, as the reader meets it. */
function control(): HTMLButtonElement {
  const button = document.querySelector<HTMLButtonElement>('.cp');
  if (button === null) throw new Error('the page drew no copy control');
  return button;
}

/** The mark the control draws, which is what says what the click did. */
function mark(): string | null {
  return control().querySelector('use')?.getAttribute('href') ?? null;
}

describe('the copy control', () => {
  it('says what the click did, and forgets it when the occupant changes', async () => {
    const written: string[] = [];
    clipboard((text: string) => {
      written.push(text);
      return Promise.resolve();
    });
    const props = boxed<{ id: string }>({ id: 'd4f70669-1f2a' });
    app = mount(SessionId, { target: document.body, props });

    control().click();
    await settle();
    expect(mark(), 'a write that resolved did not say so').toBe('#i-check');
    expect(written, 'the whole id is not what was copied').toEqual(['d4f70669-1f2a']);

    // The occupant swap: the same cell, a new id underneath it.
    props.id = 'aaaa1111-2222';
    flushSync();
    expect(document.body.textContent, 'the new occupant is not drawn').toContain('aaaa1111');
    expect(mark(), 'the control vouched for an id the row no longer shows').toBe('#i-copy');
  });

  /**
   * A write is issued for the id the row was showing, and it can settle after
   * an occupant swap has already put a new one there. A resolve that does not
   * match the id on the row must change nothing, or the control vouches for a
   * string the clipboard holds and the row no longer shows.
   */
  it('ignores a write that settles after the occupant changed', async () => {
    // In a box, because the assignment happens inside the promise's own
    // executor and a plain `let` reads as `null` to the type checker here.
    const gate: { land: (() => void) | null } = { land: null };
    clipboard(
      () =>
        new Promise<void>((resolve) => {
          gate.land = resolve;
        }),
    );
    const props = boxed<{ id: string }>({ id: 'd4f70669-1f2a' });
    app = mount(SessionId, { target: document.body, props });

    control().click();

    // The swap lands first, and the reset has already run.
    props.id = 'aaaa1111-2222';
    flushSync();
    expect(mark(), 'the reset did not run on the swap').toBe('#i-copy');

    // Now the write the FIRST occupant's row issued comes back.
    gate.land?.();
    await settle();
    expect(mark(), 'a write issued for the previous id vouched for the new one').toBe('#i-copy');
  });

  /**
   * The other arm of the same guard. A refusal that lands after a swap is
   * about the occupant the row has already left, so it must no more change the
   * control's mark than a resolve would.
   */
  it('ignores a refused write that settles after the occupant changed', async () => {
    const gate: { refuse: (() => void) | null } = { refuse: null };
    clipboard(
      () =>
        new Promise<void>((_resolve, reject) => {
          gate.refuse = () => reject(new Error('refused'));
        }),
    );
    const props = boxed<{ id: string }>({ id: 'd4f70669-1f2a' });
    app = mount(SessionId, { target: document.body, props });

    control().click();
    props.id = 'aaaa1111-2222';
    flushSync();

    gate.refuse?.();
    await settle();
    expect(mark(), 'a refusal issued for the previous id spoke for the new one').toBe('#i-copy');
  });

  it('says the write was refused rather than passing as a click that worked', async () => {
    clipboard(() => Promise.reject(new Error('refused')));
    app = mount(SessionId, { target: document.body, props: { id: 'd4f70669' } });

    control().click();
    await settle();
    expect(mark(), 'a refused write drew the same mark as a done one').toBe('#i-x');
    expect(control().getAttribute('aria-label')).toContain('refused');
  });

  it('names the missing clipboard when the page has none', () => {
    vi.stubGlobal('navigator', {});
    app = mount(SessionId, { target: document.body, props: { id: 'd4f70669' } });

    control().click();
    flushSync();
    expect(mark(), 'a page with no clipboard drew the same mark as a done write').toBe('#i-x');
    expect(
      control().getAttribute('aria-label'),
      'the name says nothing about the reason',
    ).toContain('no clipboard');
  });
});

/** A row for the fixture's lead, in the state named. */
function row(label: string, lifecycle: AgentRow['lifecycle']): AgentRow {
  const first = homeWire.agents[0];
  if (first === undefined) throw new Error('the fixture holds no agent');
  return { ...first, slot: { ...first.slot, label }, label, lifecycle, pending: null };
}

const sleepingRows = () => {
  const home: HomeWire = {
    ...homeWire,
    agents: [row('lead', 'Running'), row('slept-1', 'Sleeping'), row('slept-2', 'Sleeping')],
  };
  const project = railGroups(home, { org: 'TestOrg', project: 'proj', label: 'lead' }, 0).flatMap(
    (group) => group.projects,
  )[0];
  return project?.sleeping ?? [];
};

/** Whether the fold drawn under the sheet is open. */
function foldOpen(): boolean {
  const fold = document.querySelector<HTMLDetailsElement>('details');
  if (fold === null) throw new Error('the test drew no fold');
  return fold.open;
}

describe('a fold that holds the seat being shown', () => {
  /**
   * An occupant swap and a deep link both reach a fold that is already on
   * screen, so a fold that read its prop once would stay shut over the row a
   * reader arrived on. It opens when the seat moves in, and nothing but the
   * reader's own toggle closes it.
   */
  it("opens when the shown seat moves into a project's sleeping seats", () => {
    const props = boxed<{ sleeping: ReturnType<typeof sleepingRows>; shown: string | null }>({
      sleeping: sleepingRows(),
      shown: null,
    });
    app = mount(SleeperFold, { target: document.body, props });
    expect(foldOpen(), 'the fold opened over a seat it does not hold').toBe(false);

    props.shown = 'slept-2';
    flushSync();
    expect(foldOpen(), 'the fold stayed shut over the seat being shown').toBe(true);
  });

  it('opens when the shown seat moves into a folded group', () => {
    const props = boxed<{
      heading: string;
      count: number;
      holds: boolean;
      children: ReturnType<typeof createRawSnippet>;
    }>({
      heading: 'asleep',
      count: 3,
      holds: false,
      // The rows the group holds are the rail's; this test is about the fold.
      children: createRawSnippet(() => ({ render: () => '<span></span>' })),
    });
    app = mount(GroupFold, { target: document.body, props });
    expect(foldOpen()).toBe(false);

    props.holds = true;
    flushSync();
    expect(foldOpen(), 'the group fold stayed shut over the seat being shown').toBe(true);
  });
});
