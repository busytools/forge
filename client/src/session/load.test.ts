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

/**
 * Every record the reader produces, frozen to its leaves.
 *
 * **This is the assertion the raw read rests on.** A record held in `$state.raw`
 * cannot be mutated in place: a write would be invisible to the page, where a
 * proxied record would have redrawn. No path may write to one, and this turns
 * that from a sentence in a commit message into something the suite runs - ESM
 * is strict mode, so the first write to a frozen record throws.
 */
function frozen<T>(value: T): T {
  if (value !== null && typeof value === 'object') {
    for (const inner of Object.values(value)) frozen(inner);
    Object.freeze(value);
  }
  return value;
}

vi.mock('./wire', async (importOriginal) => {
  const real = await importOriginal<typeof import('./wire')>();
  const { counting } = await import('./testing/counts');
  return {
    ...real,
    // **Every record this file runs against is frozen to its leaves**, so the
    // whole suite carries the assertion the raw read rests on: a record held in
    // `$state.raw` cannot be mutated in place, and ESM's strict mode turns the
    // first write into a thrown TypeError rather than a silent no-op.
    sessionFrom: counting('sessionFrom', (data: unknown) => frozen(real.sessionFrom(data))),
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
 * Every frame the load capture carries, counted off the fixture itself.
 *
 * **The denominator, and it has to come from the file rather than from the
 * run.** The capture's whole value is its volume, and a call count cannot see
 * it: a `seats()` mutated to hand the page an empty conversation still calls
 * every builder once. Asserting this against the fixture's own rows is what
 * makes "the page did the work" a fact rather than an assumption.
 */
const MESSAGES = load.turns.reduce((total, turn) => total + turn.messages.length, 0);

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
      // A re-answer is a whole new record, which is what a read costs a page:
      // every slice arrives as a fresh object, so every reader of one re-runs.
      held = seats(turns);
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
    /**
     * One frame for this seat, which is what a new block or a tool result is.
     *
     * **It carries a message, because the page applies it rather than reading
     * the seat again.** A frame the fold draws nothing out of moves nothing,
     * so a case that measures what a frame costs has to send one that does.
     */
    update: (msg: unknown = block()) =>
      emit({ kind: 'update', update: { chat_appended: { key: LEAD, msg } } }),
    /** One frame for a seat this page is not looking at. */
    other: () => emit({ kind: 'update', update: { chat_appended: { key: OTHER, msg: block() } } }),
    /** One frame that names no seat at all. */
    unnamed: () => emit({ kind: 'update', update: 'busy' }),
  };
}

/** A frame the fold draws something out of: one line of what the seat said. */
function block(text = 'hello'): Record<string, unknown> {
  return {
    type: 'assistant',
    uuid: `a${text}`,
    message: { role: 'assistant', content: [{ type: 'text', text }] },
    session_id: 's',
    parent_tool_use_id: null,
  };
}

/** A frame the inspector's dispatch scan reads: a sub-agent call. */
function dispatched(): Record<string, unknown> {
  return {
    type: 'assistant',
    uuid: 'a-dispatch',
    message: {
      role: 'assistant',
      content: [{ type: 'tool_use', id: 'tu1', name: 'Task', input: {} }],
    },
    session_id: 's',
    parent_tool_use_id: null,
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

/** One arriving frame, and the flush that lets the page draw what it moved. */
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
    // Read before the two controls clear the counter: a `widthOf` taken after
    // them would be summing an empty array and asserting the zero it found.
    const scanned = counts.widthOf('hasDispatches');
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
    // **The frame is applied, not answered with a read.** The whole point of
    // the arrangement is that a page follows a busy seat without asking the
    // server to fold the transcript again, and this is the count that says so.
    expect(called('sessionFrom'), `the frame re-read the seat: ${measured}`).toBe(0);
    expect(called('projectOf'), `the home did not move, so nothing re-reads it: ${measured}`).toBe(
      0,
    );
    expect(called('gitSection'), measured).toBe(1);
    // Once, not once per section: the flatten is the whole conversation, so a
    // second reader would pay for the same walk again.
    expect(called('hasDispatches'), measured).toBe(1);
    expect(called('framesOf'), measured).toBe(1);
    // And over the whole conversation, which the call count above cannot say:
    // every number in this file is about a record of a real size, and a fixture
    // that stopped carrying one would leave all of them green. The frame above
    // is part of that conversation now, because the page put it there.
    expect(scanned, `the conversation reached the scan: ${measured}`).toBe(MESSAGES + 1);
    expect(MESSAGES, 'the capture carries no conversation').toBeGreaterThan(0);
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
    // shut, so the arithmetic its summary needs is what may still run - and
    // only where the slice it states moved. A frame carrying a message moves
    // the conversation, which is what the mcp and git readers walk; it says
    // nothing about the monitors, so that section is not recomputed at all.
    expect(called('mcpSection'), measured).toBeGreaterThan(0);
    expect(
      called('monitorsSection'),
      `${measured} - a shut section was recomputed for a slice that did not move`,
    ).toBe(0);

    // The other half of the claim: the deferral is not a section that never
    // draws. Opening one draws the body it was holding back.
    openSection('processes');
    const opened = drawn().find((section) => section.key === 'sec-processes');
    expect(opened?.body, `opening drew nothing: ${JSON.stringify(drawn())}`).toBeGreaterThan(0);
    expect(counts.countOf('walkedNote'), measured).toBeGreaterThan(0);
  });

  /**
   * **A frame is applied to the record the page holds, so what it carries has
   * to reach what is drawn.** That is the assertion the whole arrangement
   * rests on: a page that applied updates into a tree nothing reads would draw
   * a seat frozen at its last read.
   */
  it('draws what the frame it was sent actually carries', () => {
    // A seat that has dispatched nothing, so the section is absent to start
    // with and the frame has something to move.
    const server = open([]);
    counts.clear();

    const before = drawn().map((section) => section.key);
    arrive(() => server.update(dispatched()));

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
