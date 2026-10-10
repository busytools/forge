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

import { browserInflight } from '../browser/inflight.svelte';
import { subjectKey, type ServerMessage } from '../protocol';
import { onRefusal, type Refusal } from '../refusals';
import type { Connection } from '../socket';
import type { SessionSlot } from '../wire/types';
import { connectRust } from './rust';

const slot: SessionSlot = { org: 'Busytools', project: 'forge', label: 'lead' };
const subject = { session: slot };

/** A client_state answer, overridden per test. */
type State = { status: string; greeting: unknown; role: boolean };
const OPEN: State = { status: 'open', greeting: null, role: false };

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
    return Promise.resolve(undefined);
  });
  const connection = connectRust('ws://127.0.0.1:7777/socket');
  await vi.waitFor(() => expect(handlers.size).toBe(5));
  await vi.waitFor(() =>
    expect(invoke).toHaveBeenCalledWith('client_connect', { url: 'ws://127.0.0.1:7777/socket' }),
  );
  return connection;
}

afterEach(() => {
  invoke.mockReset();
  handlers.clear();
  listen.mockClear();
  browserInflight.calls = 0;
  browserInflight.visible = false;
  vi.useRealTimers();
});

describe('what the shell is told', () => {
  it('a subscribe, an unsubscribe and a refresh each cross as their own command', async () => {
    const connection = await opened();
    connection.subscribe(subject, { answering: true, browser: true });
    expect(invoke).toHaveBeenCalledWith('client_subscribe', {
      what: subject,
      answering: true,
      browser: true,
    });
    connection.unsubscribe(subject);
    expect(invoke).toHaveBeenCalledWith('client_unsubscribe', { what: subject });
    connection.refresh(subject);
    expect(invoke).toHaveBeenCalledWith('client_refresh', { what: subject });
  });

  /** A one-sided rename of a command name is exactly what this file catches. */
  it('the closed check gates what crosses', async () => {
    const connection = await opened();
    connection.close();
    expect(invoke, 'the close crossed').toHaveBeenCalledWith('client_close');
    connection.refresh(subject);
    connection.more(slot);
    expect(
      invoke,
      'nothing more was asked once the connection was closed',
    ).not.toHaveBeenCalledWith('client_refresh', expect.anything());
    expect(connection.more(slot), 'and a page is not promised').toBe(false);
  });
});

describe('the four commands that answer through a reply', () => {
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

  it('a snapshot seeds its store, and a refusal lands in it', async () => {
    const connection = await opened();
    const store = connection.subscribe(subject);
    fire('client://inbound', { kind: 'snapshot', subject, data: { header: {} } });
    expect(store.snapshot()).toEqual({ header: {} });
    expect(store.state()).toEqual({ kind: 'ready' });
    fire('client://refused', { key: subjectKey(subject), why: 'no session there' });
    expect(store.state(), 'a refused subscribe is an answer with its own words').toEqual({
      kind: 'refused',
      why: 'no session there',
    });
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
        return Promise.resolve({ status: 'open', greeting, role: false });
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

  it('a status move reaches onStatus, and the role reaches onBrowserRole once', async () => {
    let status = 'connecting';
    invoke.mockImplementation((command: string) => {
      if (command === 'client_state') {
        return Promise.resolve({ status, greeting: null, role: false });
      }
      return Promise.resolve(undefined);
    });
    const connection = connectRust('ws://127.0.0.1:7777/socket');
    await vi.waitFor(() => expect(handlers.size).toBe(5));
    const moves: string[] = [];
    connection.onStatus((next) => moves.push(next));
    status = 'open';
    fire('client://connection', undefined);
    await vi.waitFor(() => expect(moves).toEqual(['open']));
    const roles: boolean[] = [];
    connection.onBrowserRole((hosting) => roles.push(hosting));
    fire('client://role', true);
    expect(connection.browserRole(), "the role is the connection's own fact").toBe(true);
    fire('client://role', true);
    expect(roles, 'the same role twice is one move').toEqual([true]);
  });

  it("the ask count drives the strip's ring", async () => {
    await opened();
    fire('client://asks', 2);
    expect(browserInflight.calls, 'two asks in flight').toBe(2);
    expect(browserInflight.visible).toBe(true);
    fire('client://asks', 1);
    expect(browserInflight.calls, 'one landed').toBe(1);
    fire('client://asks', 0);
    expect(browserInflight.calls, 'and the last one').toBe(0);
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
