// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { writable } from 'svelte/store';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { homeWire } from '../dev/fixture.data';
import session from '../dev/fixtures/session.json';
import load from '../dev/fixtures/session-load.json';
import type { ServerMessage, Subject } from '../protocol';
import type { Connection } from '../socket';
import type { SessionSlot } from '../wire/types';

/**
 * Every builder the inspector reaches through is wrapped, so what one arriving
 * frame costs is a count rather than an impression.
 *
 * The wrappers live in the mock factories because that is the only place a
 * module's exports can be replaced, and they record into `./testing/counts`
 * because a `vi.mock` factory is hoisted above this file's imports and cannot
 * close over anything declared here.
 */
vi.mock('./view', async (importOriginal) => {
  const real = await importOriginal<typeof import('./view')>();
  const { counting } = await import('./testing/counts');
  return {
    ...real,
    gitSection: counting('gitSection', real.gitSection),
    projectOf: counting('projectOf', real.projectOf),
    mcpSection: counting('mcpSection', real.mcpSection),
    tasksSection: counting('tasksSection', real.tasksSection),
    schedulesSection: counting('schedulesSection', real.schedulesSection),
    monitorsSection: counting('monitorsSection', real.monitorsSection),
    processTree: counting('processTree', real.processTree),
    walkedNote: counting('walkedNote', real.walkedNote),
    hasDispatches: counting('hasDispatches', real.hasDispatches),
  };
});

vi.mock('./wire', async (importOriginal) => {
  const real = await importOriginal<typeof import('./wire')>();
  const { counting } = await import('./testing/counts');
  return {
    ...real,
    sessionFrom: counting('sessionFrom', real.sessionFrom),
    // The flatten is the whole conversation as one array, and it is handed to
    // a scan that stops at the first dispatch.
    framesOf: counting('framesOf', real.framesOf),
  };
});

const { default: Session } = await import('./Session.svelte');
const counts = await import('./testing/counts');

const LEAD: SessionSlot = { org: 'TestOrg', project: 'proj', label: 'lead' };
const OTHER: SessionSlot = { org: 'TestOrg', project: 'proj', label: 'worker' };
const SUBJECT: Subject = { session: LEAD };

/** The seat's record: the dev fixture's frame, the load capture's content. */
function seats(turns: unknown[]): unknown {
  return { ...session, conversation: { turns, compaction_count: 0 }, ...load };
}

/**
 * A connection that answers one seat the way the server does: a refresh
 * replaces what the store holds and then announces the new snapshot.
 *
 * `refresh` is what a read looks like from this side, so it is where the
 * denominator stays honest - a stub that replayed the same bytes without
 * counting them as a read would report the cost of a page that never re-read.
 */
function seat(turns: unknown[] = load.turns) {
  const listeners = new Set<(message: ServerMessage) => void>();
  const asked: Subject[] = [];
  let held: unknown = seats(turns);
  let answered: unknown = seats(turns);

  const emit = (message: ServerMessage): void => {
    for (const listener of listeners) listener(message);
  };

  const store = {
    subject: SUBJECT,
    value: writable({ snapshot: held, updates: [], state: { kind: 'ready' }, dropped: 0 }),
    snapshot: () => held,
    updates: () => [],
    state: () => ({ kind: 'ready' }) as const,
    dropped: () => 0,
    set: () => undefined,
    push: () => undefined,
    refuse: () => undefined,
  };

  const connection = {
    subscribe: () => store,
    unsubscribe: () => undefined,
    refresh: (what: Subject) => {
      asked.push(what);
      held = answered;
      emit({ kind: 'snapshot', subject: what, data: held });
    },
    dispatch: () => null,
    more: () => false,
    onMessage: (fn: (message: ServerMessage) => void) => {
      listeners.add(fn);
      return () => listeners.delete(fn);
    },
    onStatus: () => () => undefined,
    store: () => undefined,
    settings: () => null,
    status: () => 'open' as const,
    close: () => undefined,
  };

  return {
    connection: connection as unknown as Connection,
    asked,
    /** What the next read answers with, for a frame that changed the seat. */
    next: (value: unknown) => {
      answered = value;
    },
    /** One frame for this seat, which is what a new block or a tool result is. */
    update: () => emit({ kind: 'update', update: { chat_appended: { key: LEAD } } }),
    /** One frame for a seat this page is not looking at. */
    other: () => emit({ kind: 'update', update: { chat_appended: { key: OTHER } } }),
    /** One frame that names no seat at all. */
    unnamed: () => emit({ kind: 'update', update: 'busy' }),
  };
}

let app: Record<string, unknown> | null = null;

/** Mount the page against a seat, and settle the read it makes on the way up. */
function open(turns: unknown[] = load.turns): ReturnType<typeof seat> {
  const server = seat(turns);
  app = mount(Session, {
    target: document.body,
    props: { slot: LEAD, connection: server.connection, wire: homeWire },
  });
  flushSync();
  return server;
}

/** One arriving frame, driven through the coalescing window to its read. */
function arrive(frame: () => void): void {
  frame();
  vi.advanceTimersByTime(50);
  flushSync();
}

/** Every section the inspector drew: its name, whether it is open, and what
 * it built behind the summary. */
function drawn(): { key: string; open: boolean; body: number }[] {
  return [...document.querySelectorAll('details.sec')].map((element) => {
    const section = element as HTMLDetailsElement;
    return {
      key: section.getAttribute('data-k') ?? '',
      open: section.open,
      body: element.querySelector('.sb')?.querySelectorAll('*').length ?? 0,
    };
  });
}

/** Close every section the way a reader does, and settle the binding. */
function collapseAll(): void {
  for (const element of document.querySelectorAll('details.sec')) {
    (element as HTMLDetailsElement).open = false;
    element.dispatchEvent(new Event('toggle'));
  }
  flushSync();
}

/** Open one section, so its body is drawn. */
function openSection(name: string): void {
  const found = document.querySelector(`details.sec[data-k="sec-${name}"]`);
  if (!(found instanceof HTMLDetailsElement)) throw new Error(`no ${name} section was drawn`);
  found.open = true;
  found.dispatchEvent(new Event('toggle'));
  flushSync();
}

beforeEach(() => {
  counts.clear();
  vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] });
});

afterEach(async () => {
  vi.useRealTimers();
  if (app !== null) await unmount(app);
  app = null;
  document.body.innerHTML = '';
});

describe('what one arriving frame costs the inspector', () => {
  /**
   * **The instrument's own negative is inside the run it measures.** A frame
   * for this seat re-derives `gitSection`; the same flush re-derives
   * `projectOf` not at all, because `projectOf` reads the home and the home did
   * not move. A counter wired to renders rather than to re-derivations reports
   * both, so the pair is what makes the number below readable.
   *
   * The two frames the page should ignore are the second control: one for
   * another seat, one naming no seat. Both are routes the server really sends -
   * a connection carries the whole fleet's updates and filters them here.
   */
  it('re-derives what a frame for this seat moved, and nothing for a frame it did not', () => {
    const server = open();
    // The page's own read on the way up is not an arriving frame, so the
    // measurement starts after it.
    counts.clear();

    arrive(server.update);
    const event = counts.tally();
    const called = (name: string): number => event.find((row) => row.name === name)?.calls ?? 0;

    counts.clear();
    arrive(server.other);
    const other = counts.tally();

    counts.clear();
    arrive(server.unnamed);
    const unnamed = counts.tally();

    expect(other, 'a frame for another seat reached this page').toEqual([]);
    expect(unnamed, 'a frame naming no seat reached this page').toEqual([]);

    const measured = JSON.stringify({ event, other, unnamed });
    expect(called('sessionFrom'), measured).toBe(1);
    expect(called('projectOf'), `the home did not move, so nothing re-reads it: ${measured}`).toBe(
      0,
    );
    expect(called('gitSection'), measured).toBe(1);
    // Once, not once per section: the flatten is the whole conversation, so a
    // second reader would pay for the same walk again.
    expect(called('hasDispatches'), measured).toBe(1);
    expect(called('framesOf'), measured).toBe(1);
  });

  /**
   * **A section nobody has opened owes its name and its count, and nothing
   * else.** It used to build its whole body anyway, and the body is where the
   * inspector's cost was: a process tree, a monitor list, a hundred elements
   * behind a `<details>` that is shut.
   *
   * The count is the line. A summary's own arithmetic still runs - a summary
   * that stated a stale number would be a worse defect than a slow one - and
   * everything that exists only to fill the body does not.
   */
  it('does not compute or draw a section nobody has opened', () => {
    const server = open();
    collapseAll();
    counts.clear();

    const shut = drawn();
    expect(shut.filter((section) => section.open).length, JSON.stringify(shut)).toBe(0);
    expect(shut.length, 'the precondition: sections were drawn at all').toBeGreaterThan(0);

    arrive(server.update);
    const tally = counts.tally();
    const called = (name: string): number => tally.find((row) => row.name === name)?.calls ?? 0;

    const measured = JSON.stringify({ tally, shut });
    // `walkedNote` is the body's own: the walk's age is drawn under the tree
    // and nowhere else, so a shut section has no reason to ask for it.
    expect(called('walkedNote'), measured).toBe(0);
    // And nothing behind any of them, so what is computed is not also drawn.
    expect(
      shut.reduce((total, section) => total + section.body, 0),
      `${measured} - a shut section built elements behind its summary`,
    ).toBe(0);
    // Where the line is: a summary still states its count with every section
    // shut, so the arithmetic its summary needs is what may still run.
    expect(called('monitorsSection'), measured).toBeGreaterThan(0);

    // The other half of the claim: the deferral is not a section that never
    // draws. Opening one draws the body it was holding back.
    openSection('processes');
    const opened = drawn().find((section) => section.key === 'sec-processes');
    expect(opened?.body, `opening drew nothing: ${JSON.stringify(drawn())}`).toBeGreaterThan(0);
    expect(counts.countOf('walkedNote'), measured).toBeGreaterThan(0);
  });

  /**
   * **A read is answered with a whole new record, so nothing here is mutated
   * in place.** That is what lets the page hold its read raw instead of
   * proxying the tree - and this is the assertion that the shortcut is still a
   * live page: an event that moved the seat has to move what is drawn.
   */
  it('draws the record a frame it answered with actually carries', () => {
    // A seat that has dispatched nothing, so the section is absent to start
    // with and the frame has something to move.
    const server = open([]);
    counts.clear();

    const before = drawn().map((section) => section.key);
    server.next(
      seats([
        {
          key: null,
          messages: [
            {
              type: 'assistant',
              message: { content: [{ type: 'tool_use', id: 'tu1', name: 'Task', input: {} }] },
              parent_tool_use_id: null,
            },
          ],
        },
      ]),
    );
    arrive(server.update);

    const after = drawn().map((section) => section.key);
    const measured = JSON.stringify({ before, after });
    // Both halves, because a section that were always drawn would pass the
    // second one alone.
    expect(before, `the section was there before the frame: ${measured}`).not.toContain(
      'sec-subagents',
    );
    expect(after, `the seat's own frame did not reach the inspector: ${measured}`).toContain(
      'sec-subagents',
    );
  });
});
