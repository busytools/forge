// @vitest-environment jsdom
/**
 * The desktop's connection, over a mocked shell.
 *
 * **There is no compiler across the `invoke` boundary**, and none across the
 * event names either: the page asks the Rust half for a command or listens
 * for an event, and a one-sided rename fails at runtime as an opaque
 * "failed" naming neither side. This file drives the shim's real code against
 * a mocked shell and asserts the strings on the wire, the way
 * `browser/host.test.ts` does for the browser commands.
 *
 * The second axis is the fold: a frame from `client://inbound` has to land in
 * the stores and reach the listeners exactly as it did over the webview's own
 * socket, because every surface reads through it and a dropped arm here is a
 * page that silently stops following.
 */

import { afterEach, describe, expect, it, vi } from 'vitest';

const invoke = vi.hoisted(() =>
  vi.fn((_command: string, _args?: unknown): Promise<unknown> => Promise.resolve(undefined)),
);
const handlers = vi.hoisted(() => new Map<string, (event: { payload: unknown }) => void>());
const listen = vi.hoisted(() =>
  vi.fn((name: string, fn: (event: { payload: unknown }) => void) => {
    handlers.set(name, fn);
    return Promise.resolve(() => {
      handlers.delete(name);
    });
  }),
);

vi.mock('@tauri-apps/api/core', () => ({ invoke }));
vi.mock('@tauri-apps/api/event', () => ({ listen }));

/**
 * **The perf marks are the shim's own edges (#2513).** A snapshot's frame
 * facts and a subscribe's write arrive as shell events; these are the
 * wrappers the page records them through.
 */
vi.mock('../perf', async (importOriginal) => {
  const real = await importOriginal<typeof import('../perf')>();
  return { ...real, noteSnapshot: vi.fn(), noteSubscribeWritten: vi.fn() };
});

import { noteSnapshot, noteSubscribeWritten } from '../perf';
import { subjectKey, type ServerMessage } from '../protocol';
import { onRefusal, type Refusal } from '../refusals';
import type { BrowserFrame, Connection } from '../socket';
import { DEFAULT_SETTINGS, type SessionSlot } from '../wire/types';
import { connectRust } from './rust';

const slot: SessionSlot = { org: 'Busytools', project: 'forge', label: 'lead' };
const subject = { session: slot };

/** A client_state answer, overridden per test. */
type State = { status: string; generation?: number; greeting: unknown };
const OPEN: State = { status: 'open', generation: 1, greeting: null };
/** The id the mocked shell hands back for a `client_connect`. */
const SOCKET = 7;

/** One of the shim's events, fired where the shim listens. */
function fire(name: string, payload: unknown): void {
  const fn = handlers.get(name);
  if (fn === undefined) throw new Error(`nothing listens to ${name}`);
  fn({ payload });
}

/** A connection with its listeners in place and the shell's calls answered. */
async function opened(state: State = OPEN): Promise<Connection> {
  invoke.mockImplementation((command: string) => {
    if (command === 'client_state') return Promise.resolve(state);
    // The shell's own id for the connection it opened, which `close` names.
    if (command === 'client_connect') return Promise.resolve(SOCKET);
    return Promise.resolve(undefined);
  });
  const connection = connectRust('ws://127.0.0.1:7777/socket');
  // Six: the connection, the server's frames, the refusals, the browser's own
  // frames for the page's canvas, and #2513's frame facts and subscribe
  // writes.
  await vi.waitFor(() => expect(handlers.size).toBe(7));
  await vi.waitFor(() =>
    expect(invoke).toHaveBeenCalledWith('client_connect', { url: 'ws://127.0.0.1:7777/socket' }),
  );
  return connection;
}

afterEach(() => {
  invoke.mockReset();
  handlers.clear();
  listen.mockClear();
  vi.useRealTimers();
});

describe('what the shell is told', () => {
  it('a subscribe, an unsubscribe and a refresh each cross as their own command', async () => {
    const connection = await opened();
    connection.subscribe(subject, { answering: true, review: true });
    expect(invoke).toHaveBeenCalledWith('client_subscribe', {
      what: subject,
      answering: true,
      review: true,
    });
    connection.unsubscribe(subject);
    expect(invoke).toHaveBeenCalledWith('client_unsubscribe', { what: subject });
    connection.refresh(subject, { review: true });
    expect(invoke).toHaveBeenCalledWith('client_refresh', { what: subject, review: true });
    // **A re-ask that says nothing keeps the shell's held scope.** The bound's
    // retry for a store still loading has no page to ask, and the shell is
    // what remembers which kind of read that store was.
    connection.refresh(subject);
    expect(invoke).toHaveBeenCalledWith('client_refresh', { what: subject });
  });

  /**
   * **The shown seat crosses as its own commands.** `showing` and `relist`
   * are what separate the screen from the subscription, and `release` is the
   * one door that gives a subject back entirely; a one-sided rename of any of
   * them is silent at every layer.
   */
  it('a showing, a relist and a release each cross as their own command', async () => {
    const connection = await opened();
    await vi.waitFor(() => expect(connection.status()).toBe('open'));

    connection.showing(slot);
    expect(invoke).toHaveBeenCalledWith('client_showing', { seat: slot });
    connection.showing(null);
    expect(invoke).toHaveBeenCalledWith('client_showing', { seat: null });
    connection.relist(subject, { answering: true, review: true });
    expect(invoke).toHaveBeenCalledWith('client_relist', {
      what: subject,
      answering: true,
      review: true,
    });

    connection.subscribe(subject);
    connection.release(subject);
    expect(invoke, 'the release crossed').toHaveBeenCalledWith('client_release', {
      what: subject,
    });
    expect(connection.store(subject), 'the release left a store open on the page').toBeUndefined();
  });

  /**
   * **A rising socket generation while this page believes the connection
   * open is a reconnect it slept through.** A suspended webview wakes to a
   * socket already `open` again: every queued notice re-reads the CURRENT
   * state, `move('open')` no-ops, and without the shell's generation the
   * page would keep a frozen record for ever. A greeting cannot stand in
   * for it - the first socket's greeting arrives twice, and a reconnect to
   * one server repeats the same value.
   */
  it('fires the drop-and-open pair once for a reconnect it never saw', async () => {
    const shell: State = { status: 'open', generation: 1, greeting: null };
    const connection = await opened(shell);
    // The first connect's own read settles before the test moves the shell.
    await vi.waitFor(() => expect(connection.status()).toBe('open'));
    const seen: string[] = [];
    connection.onStatus((status) => seen.push(status));
    connection.showing(slot);
    invoke.mockClear();
    // The page's own re-attach on an open, as `session/live.ts` does it: the
    // spec's order is re-subscribe, then `Showing`.
    connection.onStatus((status) => {
      if (status === 'open') connection.relist(subject, { answering: true });
    });

    // The shell reconnected while the page was away: the generation rose,
    // the status is `open` again, and the socket's greeting crosses as a
    // frame. Two queued connection notices both read the same state.
    shell.generation = 2;
    fire('client://inbound', {
      kind: 'greeting',
      version: 12,
      settings: DEFAULT_SETTINGS,
    });
    fire('client://connection', undefined);
    fire('client://connection', undefined);

    await vi.waitFor(() => expect(seen).toEqual(['connecting', 'open']));
    await vi.waitFor(() => expect(invoke).toHaveBeenCalledWith('client_showing', { seat: slot }));
    expect(seen, 'the missed reconnect fired more than one pair').toEqual(['connecting', 'open']);
    const commands = invoke.mock.calls.map(([command]) => command);
    const relist = commands.indexOf('client_relist');
    const showing = commands.indexOf('client_showing');
    expect(relist, 'the page did not re-attach its seat on the open').toBeGreaterThanOrEqual(0);
    expect(showing, 'Showing crossed before the seat was re-subscribed').toBeGreaterThan(relist);
  });

  /**
   * **A second missed reconnect is not read as watched.** The synthesized
   * `connecting` sets the same flag a real drop does, so a pair that leaves it
   * set makes the NEXT rise compute "watched" and go unfixed - the seat
   * freezes behind an open status. It is cleared after the pair.
   */
  it('fires one pair for each of two consecutive missed reconnects', async () => {
    const shell: State = { status: 'open', generation: 1, greeting: null };
    const connection = await opened(shell);
    await vi.waitFor(() => expect(connection.status()).toBe('open'));
    const seen: string[] = [];
    connection.onStatus((status) => seen.push(status));

    shell.generation = 2;
    fire('client://connection', undefined);
    await vi.waitFor(() => expect(seen).toEqual(['connecting', 'open']));

    shell.generation = 3;
    fire('client://connection', undefined);
    await vi.waitFor(() => expect(seen).toEqual(['connecting', 'open', 'connecting', 'open']));
    expect(seen, 'a second missed reconnect did not run its own pair').toEqual([
      'connecting',
      'open',
      'connecting',
      'open',
    ]);
  });

  /** A drop the page watched fires its own pair; the generation must not add one. */
  it('does not fire a pair on a reconnect it watched', async () => {
    const shell: State = { status: 'open', generation: 1, greeting: null };
    const connection = await opened(shell);
    await vi.waitFor(() => expect(connection.status()).toBe('open'));
    const seen: string[] = [];
    connection.onStatus((status) => seen.push(status));

    shell.status = 'connecting';
    fire('client://connection', undefined);
    await vi.waitFor(() => expect(connection.status()).toBe('connecting'));
    shell.status = 'open';
    shell.generation = 2;
    fire('client://connection', undefined);
    await vi.waitFor(() => expect(connection.status()).toBe('open'));

    expect(seen, 'a watched reconnect ran the pair a second time').toEqual(['connecting', 'open']);
  });

  /** The first connect is not a reconnect, however often its greeting arrives. */
  it('does not fire a pair on the first connect', async () => {
    const connection = await opened();
    await vi.waitFor(() => expect(connection.status()).toBe('open'));
    const seen: string[] = [];
    connection.onStatus((status) => seen.push(status));
    const greeting = { kind: 'greeting', version: 1, settings: DEFAULT_SETTINGS } as const;
    fire('client://inbound', greeting);
    fire('client://inbound', greeting);
    await vi.waitFor(() => expect(connection.settings()).not.toBeNull());
    expect(seen, 'the first connect ran a reconnect').toEqual([]);
  });

  /**
   * **A connection given up before its dial answered is still closed.** The
   * id only exists once `client_connect` answers, so a close that ran first
   * has nothing to name; the dial's own continuation hands the socket back
   * the moment it lands rather than leaving it retrying behind the page.
   */
  it('closes a socket that answers after the page gave the connection up', async () => {
    // A property rather than a local: TypeScript narrows a local assigned
    // only inside a callback to `never` at the call below.
    const dial: { answer: ((id: number) => void) | null } = { answer: null };
    invoke.mockImplementation((command: string) => {
      if (command === 'client_connect') {
        return new Promise<number>((resolve) => {
          dial.answer = resolve;
        });
      }
      if (command === 'client_state') return Promise.resolve(OPEN);
      return Promise.resolve(undefined);
    });
    const connection = connectRust('ws://127.0.0.1:7777/socket');
    await vi.waitFor(() => expect(dial.answer).not.toBeNull());

    connection.close();
    if (dial.answer === null) throw new Error('the dial was never asked');
    dial.answer(SOCKET);
    await vi.waitFor(() => expect(invoke).toHaveBeenCalledWith('client_close', { id: SOCKET }));
  });

  /** A one-sided rename of a command name is exactly what this file catches. */
  it('the closed check gates what crosses', async () => {
    const connection = await opened();
    connection.close();
    expect(invoke, 'the close crossed').toHaveBeenCalledWith('client_close', { id: SOCKET });
    connection.refresh(subject);
    connection.more(slot);
    expect(
      invoke,
      'nothing more was asked once the connection was closed',
    ).not.toHaveBeenCalledWith('client_refresh', expect.anything());
    expect(connection.more(slot), 'and a page is not promised').toBe(false);
  });
});

describe('the five commands that answer through a reply', () => {
  it('a reply command answers with its body and every other command returns null', async () => {
    const connection = await opened();
    invoke.mockImplementation((command: string) => {
      if (command === 'client_state') return Promise.resolve(OPEN);
      if (command === 'client_dispatch') return Promise.resolve('the body');
      return Promise.resolve(undefined);
    });
    const answered = connection.dispatch({ spawn_worker: { label: 'x' } });
    expect(answered, 'a command that answers through a reply promises its body').toBeInstanceOf(
      Promise,
    );
    await expect(answered).resolves.toBe('the body');
    expect(invoke).toHaveBeenCalledWith('client_dispatch', {
      command: { spawn_worker: { label: 'x' } },
      reply: true,
    });
    expect(
      connection.dispatch({ cancel: { key: slot } }),
      'a command whose outcome rides the subscription returns null',
    ).toBeNull();
    expect(invoke).toHaveBeenCalledWith('client_dispatch', {
      command: { cancel: { key: slot } },
      reply: false,
    });
  });

  /**
   * A thread's delete has no update behind it, so the reply is the only way
   * the page learns it happened - and the server refuses the command outright
   * when it arrives without `reply_to`. The set this test exercises had
   * drifted from the server's and left exactly this command reply-less.
   */
  it('a review thread deletion carries a reply channel', async () => {
    const connection = await opened();
    invoke.mockImplementation((command: string) => {
      if (command === 'client_state') return Promise.resolve(OPEN);
      if (command === 'client_dispatch') return Promise.resolve(true);
      return Promise.resolve(undefined);
    });
    const command = {
      delete_review_thread: { project: 'forge', branch: 'b', thread_id: 't1' },
    };
    const answered = connection.dispatch(command);
    expect(answered, 'the delete promises its outcome').toBeInstanceOf(Promise);
    await expect(answered).resolves.toBe(true);
    expect(invoke).toHaveBeenCalledWith('client_dispatch', { command, reply: true });
  });

  it('a dispatch while closed throws before anything is registered, and the seat is noted', async () => {
    const connection = await opened();
    connection.close();
    const lines: Refusal[] = [];
    const stop = onRefusal((line) => lines.push(line));
    expect(
      () => connection.dispatch({ cancel: { key: slot } }, slot),
      'the throw is the only channel left for a command that never went',
    ).toThrow('the socket is not open');
    expect(lines.length, 'and the loss is drawn in the seat the click was made in').toBe(1);
    stop();
  });
});

describe('frames from the bridge', () => {
  it('a frame reaches its stores and every listener as it came', async () => {
    const connection = await opened();
    const seen: ServerMessage[] = [];
    connection.onMessage((message) => seen.push(message));
    connection.subscribe(subject);
    connection.subscribe('home');
    const update = { turn_error: { key: slot } };
    fire('client://inbound', { kind: 'update', update });
    expect(connection.store(subject)?.updates(), 'the seat store holds the frame').toEqual([
      update,
    ]);
    expect(
      connection.store('home')?.updates(),
      'and the home hears a frame its covering rule sends it',
    ).toEqual([update]);
    expect(seen, 'every listener heard the message itself').toHaveLength(1);
    expect(seen[0]?.kind).toBe('update');
    expect(
      connection.dispatch({ read_call_output: { key: slot, call_id: 'c1' } }),
      'and a command still crosses after a frame',
    ).toBeNull();
  });

  it('a snapshot seeds its store, and a refusal lands in it and reaches the page', async () => {
    const connection = await opened();
    const store = connection.subscribe(subject);
    const seen: unknown[] = [];
    connection.onMessage((message) => seen.push(message));
    fire('client://inbound', { kind: 'snapshot', subject, data: { header: {} } });
    expect(store.snapshot()).toEqual({ header: {} });
    expect(store.state()).toEqual({ kind: 'ready' });
    fire('client://refused', { key: subjectKey(subject), why: 'no session there' });
    expect(store.state(), 'a refused subscribe is an answer with its own words').toEqual({
      kind: 'refused',
      why: 'no session there',
    });
    // **The page is told as a frame**, which is what a subject store reads its
    // refused state on; without it a board stays "reading..." forever. The
    // frame names the subject it belongs to, so only that subject's readers
    // act on it.
    expect(seen.at(-1), 'the refusal did not reach the page listeners').toEqual({
      kind: 'error',
      what: 'subscribe',
      why: 'no session there',
      seat: slot,
      subject,
    });
  });

  it('names a non-seat subject on the refusal frame, and no seat', async () => {
    const connection = await opened();
    const store = connection.subscribe('board');
    const seen: unknown[] = [];
    connection.onMessage((message) => seen.push(message));
    fire('client://refused', { key: 'board', why: 'this forge holds no board' });
    expect(store.state(), 'the board store is refused').toEqual({
      kind: 'refused',
      why: 'this forge holds no board',
    });
    expect(seen.at(-1), 'the board refusal did not name its subject').toEqual({
      kind: 'error',
      what: 'subscribe',
      why: 'this forge holds no board',
      seat: null,
      subject: 'board',
    });
  });

  /**
   * **A read the server never answers is asked again**: on the phone the
   * socket lives in the Rust half, and a first snapshot lost between the
   * server and the page left the store loading with nothing behind it. The
   * re-ask is `client_refresh`, which the Rust half sends as the pair the
   * server counts evenly.
   */
  it('asks the shell again for a read the bound finds still loading', async () => {
    vi.useFakeTimers();
    invoke.mockImplementation((command: string) => {
      if (command === 'client_state') return Promise.resolve(OPEN);
      return Promise.resolve(undefined);
    });
    const connection = connectRust('ws://127.0.0.1:7777/socket', 1_000);
    await vi.waitFor(() => expect(connection.status()).toBe('open'));
    const store = connection.subscribe(subject);
    expect(invoke, 'the first ask crossed').toHaveBeenCalledWith('client_subscribe', {
      what: subject,
      answering: false,
      review: false,
    });

    vi.advanceTimersByTime(1_000);
    await vi.waitFor(() =>
      expect(invoke).toHaveBeenCalledWith('client_refresh', { what: subject }),
    );
    expect(store.state(), 'nothing has answered it').toEqual({ kind: 'loading' });

    // And an answer clears it: a read that landed owes no re-ask.
    fire('client://inbound', { kind: 'snapshot', subject, data: { header: {} } });
    await vi.waitFor(() => expect(store.state().kind).toBe('ready'));
    invoke.mockClear();
    vi.advanceTimersByTime(5_000);
    expect(invoke, 'an answered read is not re-asked').not.toHaveBeenCalledWith('client_refresh', {
      what: subject,
    });
  });

  /**
   * **A review read's deadline comes from the caller.** The server bounds that
   * read itself for longer than an ordinary one, so the ordinary pass would
   * presume it lost inside the window and ask a second whole review read under
   * the first. This drives the caller rather than arming the bound by hand, so
   * a call site reverted to the short pass reds here.
   */
  it('gives a review read the longer pass before asking again', async () => {
    vi.useFakeTimers();
    invoke.mockImplementation((command: string) => {
      if (command === 'client_state') return Promise.resolve(OPEN);
      return Promise.resolve(undefined);
    });
    const connection = connectRust('ws://127.0.0.1:7777/socket');
    await vi.waitFor(() => expect(connection.status()).toBe('open'));
    connection.subscribe(subject, { review: true });
    invoke.mockClear();

    vi.advanceTimersByTime(5_000);
    expect(invoke, 'the ordinary pass does not end a review read').not.toHaveBeenCalledWith(
      'client_refresh',
      { what: subject },
    );

    vi.advanceTimersByTime(7_000);
    await vi.waitFor(() =>
      expect(invoke).toHaveBeenCalledWith('client_refresh', { what: subject }),
    );
  });

  /**
   * **A read bound while the socket is still down owes a retry once it opens.**
   * The bound fired before the connection did, so its refresh could not go and
   * the timer died. The subscribe was queued in the Rust half and goes out when
   * the socket opens; without a fresh bound a lost first answer leaves the store
   * loading forever, which is the "reading the fleet" hang (#2540).
   */
  it('re-arms the bound when the connection opens after a read expired while connecting', async () => {
    vi.useFakeTimers();
    const connection = await opened({ status: 'connecting', greeting: null });
    const store = connection.subscribe(subject);
    expect(invoke, 'the first ask crossed').toHaveBeenCalledWith('client_subscribe', {
      what: subject,
      answering: false,
      review: false,
    });

    // The bound fires while connecting: the refresh cannot go, so the timer dies.
    vi.advanceTimersByTime(5_000);
    await vi.waitFor(() =>
      expect(invoke).not.toHaveBeenCalledWith('client_refresh', { what: subject }),
    );

    // The connection opens: the bound must be re-armed for the still-loading subject.
    invoke.mockImplementation((command: string) => {
      if (command === 'client_state') {
        return Promise.resolve({ status: 'open', generation: 1, greeting: null });
      }
      return Promise.resolve(undefined);
    });
    fire('client://connection', undefined);
    await vi.waitFor(() => expect(connection.status()).toBe('open'));

    // The re-armed bound asks again now that the socket is open.
    vi.advanceTimersByTime(5_000);
    await vi.waitFor(() =>
      expect(invoke).toHaveBeenCalledWith('client_refresh', { what: subject }),
    );
    expect(store.state(), 'nothing has answered it').toEqual({ kind: 'loading' });
  });

  /**
   * **A closed connection owes no read.** A page that goes closed keeps its
   * loading store but must not keep its bound ticking: a refresh cannot go on
   * a socket that is gone, and the next open is the one that re-arms (#2540).
   */
  it('stops the read bound when the connection closes', async () => {
    vi.useFakeTimers();
    const connection = await opened();
    const store = connection.subscribe(subject);
    invoke.mockImplementation((command: string) => {
      if (command === 'client_state') {
        return Promise.resolve({ status: 'closed', generation: 1, greeting: null });
      }
      return Promise.resolve(undefined);
    });
    fire('client://connection', undefined);
    await vi.waitFor(() => expect(connection.status()).toBe('closed'));

    vi.advanceTimersByTime(5_000);
    await vi.waitFor(() =>
      expect(invoke).not.toHaveBeenCalledWith('client_refresh', { what: subject }),
    );
    expect(store.state(), 'nothing has answered it').toEqual({ kind: 'loading' });
  });
});

describe("the connection's own facts", () => {
  it("the greeting reaches the listeners and the surfaces' own reads", async () => {
    const greeting = {
      kind: 'greeting',
      version: 7,
      forge_version: '1.1.0+abc',
      forge_version_short: '1.1.0',
      settings: { mark: 'klin', theme: 'dark', font: 'mono' },
    };
    invoke.mockImplementation((command: string) => {
      if (command === 'client_state') {
        return Promise.resolve({ status: 'open', generation: 1, greeting });
      }
      return Promise.resolve(undefined);
    });
    const connection = connectRust('ws://127.0.0.1:7777/socket');
    const kinds: string[] = [];
    connection.onMessage((message) => kinds.push(message.kind));
    await vi.waitFor(() => expect(connection.settings()).not.toBeNull());
    expect(kinds, 'the greeting crossed as a message, as it always did').toContain('greeting');
    expect(connection.settings()?.mark).toBe('klin');
    expect(connection.serverProtocol()).toBe(7);
    expect(
      connection.skew(),
      "a protocol that differs from this client's is a skew the surfaces draw",
    ).toEqual({ serverProtocol: 7, serverVersion: '1.1.0' });
  });

  /**
   * **The shell's own facts reach the perf log as their marks (#2513).** A
   * forwarded snapshot's raw length and parse, and a subscribe's write wait,
   * are measured in the Rust half; a one-sided rename here would silence the
   * switch-gap record with nothing failing.
   */
  it("the shell's frame facts and subscribe writes reach the perf log", async () => {
    await opened();
    vi.mocked(noteSnapshot).mockClear();
    vi.mocked(noteSubscribeWritten).mockClear();

    fire('client://framed', { subject, bytes: 2_200_000, parse_ms: 38.5 });
    expect(noteSnapshot).toHaveBeenCalledWith(subject, 2_200_000, 38.5);

    fire('client://written', { key: subjectKey(subject), declared: false, waited_ms: 12.5 });
    expect(noteSubscribeWritten).toHaveBeenCalledWith('Busytools/forge/lead', false, 12.5);
  });

  /**
   * **A relayed frame reaches the page's canvas with the viewport it was
   * painted from.** The shell hands it over as base64 with the header's
   * fields; a frame that names no page has nowhere to map a gesture, so it
   * is dropped rather than painted under a size this page would guess.
   */
  it('hands a relayed frame to the canvas with the page it was painted from', async () => {
    const connection = await opened();
    const heard: BrowserFrame[] = [];
    connection.onBrowserFrame((frame) => heard.push(frame));
    const bytes = btoa(String.fromCharCode(0x52, 0x49, 0x46, 0x46));

    fire('client://browser-frame', {
      profile: 5,
      tab: 2,
      seq: 7,
      width: 2560,
      height: 1440,
      format: 1,
      key: true,
      page_width: 1280,
      page_height: 720,
      bytes,
    });
    expect(heard, 'the frame reaches the canvas').toHaveLength(1);
    expect(heard[0], 'with the page it was painted from').toMatchObject({
      profile: 5,
      tab: 2,
      width: 2560,
      height: 1440,
      page_width: 1280,
      page_height: 720,
    });
    expect([...(heard[0]?.bytes ?? [])], 'and the pixels it carried').toEqual([
      0x52, 0x49, 0x46, 0x46,
    ]);

    fire('client://browser-frame', {
      profile: 5,
      tab: 2,
      seq: 8,
      width: 2560,
      height: 1440,
      format: 1,
      key: true,
      bytes,
    });
    expect(heard, 'a frame with no page is not painted').toHaveLength(1);
  });

  it('a status move reaches onStatus', async () => {
    let status = 'connecting';
    invoke.mockImplementation((command: string) => {
      if (command === 'client_state') {
        return Promise.resolve({ status, greeting: null });
      }
      return Promise.resolve(undefined);
    });
    const connection = connectRust('ws://127.0.0.1:7777/socket');
    await vi.waitFor(() => expect(handlers.size).toBe(7));
    const moves: string[] = [];
    connection.onStatus((next) => moves.push(next));
    status = 'open';
    fire('client://connection', undefined);
    await vi.waitFor(() => expect(moves).toEqual(['open']));
  });

  it("the heartbeat runs on the page's own timer", async () => {
    vi.useFakeTimers();
    const connection = await opened();
    await vi.advanceTimersByTimeAsync(5_000);
    expect(invoke).toHaveBeenCalledWith('client_heartbeat');
    connection.close();
    const heard = invoke.mock.calls.filter(([name]) => name === 'client_heartbeat').length;
    await vi.advanceTimersByTimeAsync(15_000);
    expect(
      invoke.mock.calls.filter(([name]) => name === 'client_heartbeat').length,
      'a closed connection stops beating',
    ).toBe(heard);
  });
});
