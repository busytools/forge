/**
 * The usage page's wire and a connection that drives it, both for tests.
 *
 * **Nothing the shipped app imports may import this file.** It is mock data
 * and a fake server standing where real ones stand, and the app's only input
 * is the server URL. `fixture.test.ts` builds the bundle and fails on a
 * fixture reaching it - by a marker string, and this file carries none (the
 * usage wire names no org). So the rule here is checked directly instead:
 * `testing.test.ts` sweeps the tree and fails if anything outside a test
 * imports this module.
 */

import type { Command, ServerMessage, Subject } from '../protocol';
import { PROTOCOL_VERSION } from '../protocol';
import type { Connection, ConnectionStatus } from '../socket';
import { Stores } from '../stores';
import type { UsageReport } from '../wire/usage';

/** One labelled row, with the five token fields and a cost. */
function row(
  label: string,
  input: number,
  cacheWrite1h: number,
  cacheRead: number,
  output: number,
  costUsd: number,
) {
  return {
    label,
    input,
    cache_write_1h: cacheWrite1h,
    cache_write_5m: 0,
    cache_read: cacheRead,
    output,
    cost_usd: costUsd,
  };
}

/**
 * What a forge with a couple of weeks of usage and a pricing table answers,
 * as the SERVER writes it (snake_case) - a snapshot is raw wire, and the
 * page's own narrowing is what a test then reads through.
 */
export const usageWire = {
  today: {
    by_model: [row('claude-opus-5', 200_000, 1_200_000, 22_000_000, 90_000, 11.4)],
    by_project: [row('forge', 200_000, 1_200_000, 22_000_000, 90_000, 11.4)],
    total: row('TOTAL', 200_000, 1_200_000, 22_000_000, 90_000, 11.4),
  },
  week: {
    by_model: [row('claude-opus-5', 900_000, 6_000_000, 114_000_000, 450_000, 57.5)],
    by_project: [row('forge', 900_000, 6_000_000, 114_000_000, 450_000, 57.5)],
    total: row('TOTAL', 900_000, 6_000_000, 114_000_000, 450_000, 57.5),
  },
  month: {
    by_model: [
      row('claude-opus-5', 1_400_000, 9_000_000, 170_000_000, 700_000, 86.3),
      row('gpt-5-codex', 400_000, 0, 0, 200_000, 3.6),
    ],
    by_project: [row('forge', 1_800_000, 9_000_000, 170_000_000, 900_000, 89.9)],
    total: row('TOTAL', 1_800_000, 9_000_000, 170_000_000, 900_000, 89.9),
  },
  lifetime: {
    by_model: [
      row('claude-opus-5', 2_100_000, 18_000_000, 430_000_000, 1_100_000, 240.0),
      row('gpt-5-codex', 400_000, 0, 0, 200_000, 3.6),
    ],
    by_project: [row('forge', 2_500_000, 18_000_000, 430_000_000, 1_300_000, 243.6)],
    total: row('TOTAL', 2_500_000, 18_000_000, 430_000_000, 1_300_000, 243.6),
  },
  pricing_available: true,
} as unknown as UsageReport;

/**
 * A connection a test drives by hand: what the page asked it to do, and a way
 * to hand it a frame.
 *
 * The stores are the real ones, because a subscription's lifecycle is what
 * this fakes around rather than anything about a store - and `arrive` writes
 * a snapshot into its store the way the socket does, so a read through the
 * store is the answer the server gave.
 */
export function fakeConnection() {
  const stores = new Stores();
  const subscribed: Subject[] = [];
  const unsubscribed: Subject[] = [];
  const refreshed: Subject[] = [];
  const dispatched: Command[] = [];
  const messages = new Set<(message: ServerMessage) => void>();
  const statuses = new Set<(status: ConnectionStatus) => void>();

  const connection: Connection = {
    subscribe(what) {
      subscribed.push(what);
      return stores.open(what);
    },
    unsubscribe(what) {
      unsubscribed.push(what);
      return stores.close(what);
    },
    refresh(what) {
      refreshed.push(what);
    },
    dispatch(command: Command) {
      dispatched.push(command);
      return null;
    },
    onMessage(fn) {
      messages.add(fn);
      return () => {
        messages.delete(fn);
      };
    },
    onStatus(fn) {
      statuses.add(fn);
      return () => {
        statuses.delete(fn);
      };
    },
    more: () => false,
    devices: () => false,
    commitDiff: () => false,
    readDocument: () => false,
    frame: () => false,
    store: () => undefined,
    settings: () => null,
    skew: () => null,
    serverProtocol: () => PROTOCOL_VERSION,
    status: () => 'open',
    close: () => {},
    onBrowserAsk: () => () => {},
    browserRole: () => false,
    onBrowserRole: () => () => {},
    onProfilesChanged: () => () => {},
    takeBrowserRole: () => {},
  };

  return {
    connection,
    subscribed,
    unsubscribed,
    refreshed,
    dispatched,
    /** Everything still attached to the connection. */
    listening: () => messages.size + statuses.size,
    /** One frame as the server sent it. */
    arrive(message: ServerMessage) {
      if (message.kind === 'snapshot') {
        stores.get(message.subject)?.set(message.data);
      }
      if (message.kind === 'error' && message.what === 'subscribe') {
        stores.get(USAGE)?.refuse(message.why);
      }
      for (const fn of [...messages]) fn(message);
    },
  };
}

/** The usage page's subject, which only this page watches. */
export const USAGE: Subject = 'usage';
