import { describe, expect, it } from 'vitest';

import type { ServerMessage, Subject } from '../protocol';
import { PROTOCOL_VERSION } from '../protocol';
import type { Connection, ConnectionStatus } from '../socket';
import { Stores } from '../stores';
import type { SessionSlot } from '../wire/types';
import { watchHome, type HomeRead } from './live';

const HOME: Subject = 'home';
const LEAD: SessionSlot = { org: 'TestOrg', project: 'proj', label: 'lead' };

/** An update a home subscriber is sent, which is what it answers with a read. */
const UPDATE: ServerMessage = { kind: 'update', update: { turn_cancelled: { key: LEAD } } };

/**
 * A connection a test drives by hand: what the home asked it to do, and a way
 * to hand it a frame.
 *
 * The stores are the real ones, because a subscription's lifecycle is what
 * this fakes around rather than anything about a store.
 */
function fakeConnection() {
  const stores = new Stores();
  const subscribed: Subject[] = [];
  /** The options each subscribe went out with, which the role rides. */
  const options: { answering?: boolean; browser?: boolean }[] = [];
  const unsubscribed: Subject[] = [];
  const refreshed: Subject[] = [];
  const messages = new Set<(message: ServerMessage) => void>();
  const statuses = new Set<(status: ConnectionStatus) => void>();

  const connection: Connection = {
    subscribe(what, chosen) {
      subscribed.push(what);
      options.push(chosen ?? {});
      return stores.open(what);
    },
    unsubscribe(what) {
      unsubscribed.push(what);
      return stores.close(what);
    },
    refresh(what) {
      refreshed.push(what);
    },
    onBrowserAsk: () => () => {},
    browserRole: () => false,
    onBrowserRole: () => () => {},
    takeBrowserRole: () => {},
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
    dispatch: () => null,
    more: () => false,
    devices: () => false,
    frame: () => false,
    store: () => undefined,
    settings: () => null,
    skew: () => null,
    serverProtocol: () => PROTOCOL_VERSION,
    status: () => 'open',
    close: () => {},
  };

  return {
    connection,
    subscribed,
    options,
    unsubscribed,
    refreshed,
    /** Everything still attached to the connection. */
    listening: () => messages.size + statuses.size,
    /** One frame as the server sent it, into whatever is still listening. */
    arrive(message: ServerMessage) {
      for (const fn of [...messages]) fn(message);
    },
  };
}

/** Give a timer a chance to fire, for everything it would have set off to show. */
async function settle(): Promise<void> {
  await new Promise((resolve) => setTimeout(resolve, 100));
}

describe('the home over a connection', () => {
  /**
   * **The connection declares itself answerable, usually on its first
   * subscribe.** The role belongs to the connection and only ever rises, and
   * this subscribe is usually the first the app makes (/models gets there
   * first on a deep link; a seat page waits on the home's own snapshot), so
   * declaring it here is what a page away from any seat has. It is a
   * safeguard rather than the ask's own cure - a machine running forge has
   * its terminal attached as an answerer for as long as it runs - and the
   * case it covers is a serve with no terminal anywhere.
   */
  it('declares the connection answerable, usually on its first subscribe', () => {
    const forge = fakeConnection();
    const stop = watchHome(forge.connection).subscribe(() => {});

    expect(forge.subscribed, 'the home was not subscribed').toEqual([HOME]);
    expect(forge.options[0]?.answering, 'the connection declares nothing to answer with').toBe(
      true,
    );

    stop();
  });

  /**
   * **An update that lands mid-read is not dropped (#1885).** The home
   * coalesces its reads behind one flag, and the answer in flight was encoded
   * BEFORE the update arrived - so skipping the update leaves the page drawing
   * the state it asked about until something else moves. That is the wait Ved
   * met: an ask answered, and the row still sitting under needs-you.
   */
  it('asks again for an update that lands while a read is in flight', async () => {
    const forge = fakeConnection();
    const stop = watchHome(forge.connection).subscribe(() => {});

    forge.arrive(UPDATE);
    await settle();
    expect(forge.refreshed, 'the first update did not ask for a read').toEqual(['home']);

    // The read is in flight: no answer for it has arrived yet.
    forge.arrive(UPDATE);
    await settle();
    expect(forge.refreshed, 'a second read ran while the first was in flight').toEqual(['home']);

    // Its answer lands, and the update that arrived meanwhile is asked for.
    forge.arrive({ kind: 'snapshot', subject: HOME, data: null });
    await settle();
    expect(forge.refreshed, 'the update that landed mid-read was dropped').toEqual([
      'home',
      'home',
    ]);
    stop();
  });

  /**
   * **The turn's start is a read, and #1887 is what a page that skipped it
   * drew.** A prompt's lifecycle is the earliest frame that says a turn was
   * accepted: the roster's `running` comes from `turn_pending`, stamped when
   * the prompt is routed, so a read asked at `queued` or `started` is
   * answered with the turn already on. Without it the row kept the idle it
   * last read until some unrelated frame happened to be news, measured at
   * ~40s while an agent ran underneath.
   */
  it("reads when a prompt's life moves", async () => {
    const forge = fakeConnection();
    const stop = watchHome(forge.connection).subscribe(() => {});

    forge.arrive({
      kind: 'update',
      update: { prompt_lifecycle: { key: LEAD, uuid: 'p1', state: 'started' } },
    });
    await settle();
    expect(forge.refreshed, "a turn's start asked for no read").toEqual([HOME]);
    stop();
  });

  /**
   * The background registry moves a row too - `has_background_work` is the
   * promotion that draws an idle session as running - and its read is asked
   * for through the conversation frame the same CLI event arrives in. The
   * registry's own mirror is the inspector's copy and stays out of the fleet
   * table; this is the arm that keeps a backgrounded seat from reading as
   * idle.
   */
  it('reads when the CLI announces background work', async () => {
    const forge = fakeConnection();
    const stop = watchHome(forge.connection).subscribe(() => {});

    forge.arrive({
      kind: 'update',
      update: {
        chat_appended: {
          key: LEAD,
          msg: { type: 'system', subtype: 'background_tasks_changed', tasks: [] },
        },
      },
    } as unknown as ServerMessage);
    await settle();
    expect(forge.refreshed, 'a background task starting asked for no read').toEqual([HOME]);
    stop();
  });

  /**
   * The core's service line reaches the page's own store, not only a re-read:
   * the snapshot holds no such field, so the report is what a page has to draw
   * a refused edit with - and a report dropped here is a refusal that reads as
   * an edit that landed.
   */
  it("carries the core's service report into the next read", async () => {
    const forge = fakeConnection();
    const seen: HomeRead[] = [];
    const stop = watchHome(forge.connection).subscribe((read) => seen.push(read));

    forge.arrive({
      kind: 'update',
      update: {
        service_status: { severity: 'warning', message: 'The board refused a move: nope' },
      },
    });
    await settle();
    // The read the update asked for answers with the words it carried.
    forge.arrive({ kind: 'snapshot', subject: HOME, data: null });
    await settle();

    const last = seen.at(-1);
    expect(last?.report, 'the report never reached the store').toEqual({
      severity: 'warning',
      message: 'The board refused a move: nope',
    });
    stop();
  });

  /**
   * The other side of it: a frame the home does not cover is no reason to
   * read. A chat message moves no row, so the page must not ask again for it.
   */
  it('does not read for a frame the home does not cover', async () => {
    const forge = fakeConnection();
    const stop = watchHome(forge.connection).subscribe(() => {});

    // Cast: this drives the door rather than the wire's narrowing, and only
    // the variant name is read at it.
    forge.arrive({
      kind: 'update',
      update: { chat_appended: { key: LEAD, msg: { type: 'assistant' } } },
    } as unknown as ServerMessage);
    await settle();
    expect(forge.refreshed, 'a chat message asked for a home read').toEqual([]);
    stop();
  });

  /**
   * A read is a full encode on the server, so a subscription nobody draws from
   * must not be left asking for one. The last subscriber leaving is what
   * releases the subscription, the listeners and any read already queued: a
   * replaced connection otherwise stays counted against a seat it is not
   * showing, re-reading the fleet for a page that is gone.
   */
  it('stops asking for the home once the last subscriber goes', async () => {
    const forge = fakeConnection();
    const stop = watchHome(forge.connection).subscribe(() => {});

    // While it is watched: an update is answered with one read, and the answer
    // to that read is what lets the next one be asked for.
    forge.arrive(UPDATE);
    await settle();
    expect(forge.refreshed, 'a watched home did not answer an update with a read').toEqual([
      'home',
    ]);
    forge.arrive({ kind: 'snapshot', subject: HOME, data: null });

    // The release, with a read already queued behind the last subscriber.
    forge.arrive(UPDATE);
    stop();
    expect(forge.unsubscribed, 'the last subscriber left the home subscribed').toEqual(['home']);

    // The queued read does not go, and no frame after it can ask for one.
    await settle();
    expect(forge.refreshed, 'a read queued behind the last subscriber still went').toHaveLength(1);
    forge.arrive(UPDATE);
    await settle();
    expect(forge.refreshed, 'a frame still reached a released subscription').toHaveLength(1);

    // And nothing is left attached to the connection, the status listener
    // included.
    expect(forge.listening(), 'the release left a listener on the connection').toBe(0);
  });
});
