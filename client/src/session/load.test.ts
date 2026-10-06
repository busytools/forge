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
import { freeze } from '../chat/testing/frozen';

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
    tasksSection: counting('tasksSection', real.tasksSection),
    monitorsSection: counting('monitorsSection', real.monitorsSection),
  };
});

vi.mock('./wire', async (importOriginal) => {
  const real = await importOriginal<typeof import('./wire')>();
  const { counting } = await import('./testing/counts');
  return {
    ...real,
    // **Every record this file runs against is frozen to its leaves**, so the
    // whole suite carries the assertion the raw read rests on: a record held in
    // `$state.raw` cannot be mutated in place, and ESM's strict mode turns the
    // first write into a thrown TypeError rather than a silent no-op.
    sessionFrom: counting('sessionFrom', (data: unknown) => freeze(real.sessionFrom(data))),
  };
});

const { default: Session } = await import('./Session.svelte');
const counts = await import('./testing/counts');

const LEAD: SessionSlot = { org: 'TestOrg', project: 'proj', label: 'lead' };
const OTHER: SessionSlot = { org: 'TestOrg', project: 'proj', label: 'worker' };
const SUBJECT: Subject = { session: LEAD };

/** The seat's record: the dev fixture's frame, the load capture's content, and
 * whatever a case wants the record itself to say. */
function seats(turns: unknown[], fields: Record<string, unknown> = {}): unknown {
  return { ...session, conversation: { turns, compaction_count: 0 }, ...load, ...fields };
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
function seat(turns: unknown[] = load.turns, fields: Record<string, unknown> = {}) {
  const listeners = new Set<(message: ServerMessage) => void>();
  const asked: Subject[] = [];
  let held: unknown = seats(turns, fields);

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
      held = seats(turns, fields);
      emit({ kind: 'snapshot', subject: what, data: held });
    },
    dispatch: () => null,
    more: () => false,
    onMessage: (fn: (message: ServerMessage) => void) => {
      listeners.add(fn);
      // **The subscription's own answer**, which the socket sends once the
      // subscribe is taken. Without it the seat's whole-record ask is never
      // spent and every later ask returns before it asks, so nothing in this
      // file can see a read.
      emit({ kind: 'snapshot', subject: SUBJECT, data: held });
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
    /** One pushed process walk for this seat, the frame a held seat's own loop sends. */
    walk: (snapshot: unknown) =>
      emit({ kind: 'update', update: { processes_changed: { key: LEAD, snapshot } } }),
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

let app: Record<string, unknown> | null = null;

/** Mount the page against a seat, and settle the read it makes on the way up. */
function open(
  turns: unknown[] = load.turns,
  fields: Record<string, unknown> = {},
): ReturnType<typeof seat> {
  const server = seat(turns, fields);
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
  // The page schedules paints on animation frames, and a case that drives one
  // needs a clock it can advance.
  vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout', 'setInterval', 'clearInterval'] });
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
    // Read before the clear below: what the assertion is about is the record
    // the page's own read was handed, and that read is the one thing here that
    // is not an arriving frame.
    const rows = counts.rowsCarried('sessionFrom');
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
    // **The frame is applied, not answered with a read.** The whole point of
    // the arrangement is that a page follows a busy seat without asking the
    // server to fold the transcript again, and this is the count that says so.
    expect(called('sessionFrom'), `the frame re-read the seat: ${measured}`).toBe(0);
    expect(called('projectOf'), `the home did not move, so nothing re-reads it: ${measured}`).toBe(
      0,
    );
    expect(called('gitSection'), measured).toBe(1);
    // And over a record of a real size, which the call counts cannot say: every
    // number in this file is about a conversation the record really carries, and
    // a fixture that stopped carrying one would leave all of them green.
    expect(rows, `the rows the record handed the page: ${measured}`).toBe(MESSAGES);
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
    // Nothing behind any of them, so what is computed is not also drawn.
    expect(
      shut.reduce((total, section) => total + section.body, 0),
      `${measured} - a shut section built elements behind its summary`,
    ).toBe(0);
    // Where the line is: a summary still states its count with every section
    // shut, so the arithmetic its summary needs is what may still run - and
    // only where the slice it states moved. A frame carrying a message moves
    // the record, which is what the git reader walks; it says nothing about
    // the monitors, so that section is not recomputed at all.
    expect(called('gitSection'), measured).toBeGreaterThan(0);
    expect(
      called('monitorsSection'),
      `${measured} - a shut section was recomputed for a slice that did not move`,
    ).toBe(0);

    // The other half of the claim: the deferral is not a section that never
    // draws. Opening one draws the body it was holding back.
    openSection('monitors');
    const opened = drawn().find((section) => section.key === 'sec-monitors');
    expect(opened?.body, `opening drew nothing: ${JSON.stringify(drawn())}`).toBeGreaterThan(0);
  });

  /**
   * **A pushed walk no longer draws a section.** It used to be the inspector's
   * processes section; the walk is the strip's row now, so the claim that
   * survives the removal is the frame reaching the inspector as NOTHING - no
   * section, and no re-read of the seat behind it.
   */
  it('lets a pushed walk pass the inspector without drawing or re-reading', () => {
    const fields: Record<string, unknown> = {
      processes: { processes: [], scanned_at: { secs_since_epoch: 0, nanos_since_epoch: 0 } },
    };
    const server = open([], fields);
    expect(drawn().map((section) => section.key)).not.toContain('sec-processes');

    arrive(() =>
      server.walk({
        processes: [
          { pid: 4, parent_pid: 1, name: 'claude', command: 'claude', memory_bytes: 1024 },
        ],
        scanned_at: { secs_since_epoch: 1, nanos_since_epoch: 0 },
      }),
    );

    const keys = drawn().map((section) => section.key);
    expect(keys, `a pushed walk drew a section again: ${JSON.stringify(keys)}`).not.toContain(
      'sec-processes',
    );
    expect(server.asked.length, 'the frame re-read the seat').toBe(0);
  });
});

/**
 * **A usage nothing has reported is not a usage of zero.** The two draw the
 * same the moment an empty track stands in for an unknown one: a bar at 0% with
 * a dash beside it reads as a session that has used no context, where the truth
 * is that nobody has asked, and only the track says which.
 */
describe("the header's context usage", () => {
  /** The header's context cell, as the page drew it. */
  const cell = (): Element | null => document.querySelector('.facts .cm');

  it('draws no bar for a usage nothing has reported', () => {
    open([], { header: { ...session.header, context: { percent: null, max_tokens: null } } });

    expect(
      cell()?.querySelector('.tk'),
      'an unknown usage drew a track, which reads as a usage of zero',
    ).toBeNull();
    expect(cell()?.textContent, 'and it drew no marker of its own').toContain('\u{2014}');
  });

  it('draws the bar for a usage that is really zero', () => {
    open([], { header: { ...session.header, context: { percent: 0, max_tokens: 1_048_576 } } });

    expect(
      cell()?.querySelector('.tk'),
      'a real zero drew no track, which reads as a usage nobody has reported',
    ).not.toBeNull();
    expect(cell()?.textContent).toContain('0%');
  });
});
