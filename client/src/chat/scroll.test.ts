// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import type { ClientMessage, ServerMessage } from '../protocol';
import type { Connection } from '../socket';
import type { SessionSlot } from '../wire/types';

/**
 * The list is a stub, because what has to be seen is what the column HANDS it.
 *
 * `virtua` reads the compensation once, as the row count changes, and consults
 * it nowhere else - so the difference between armed and not is one frame, and
 * no browser measurement holds a frame. The stub reads it in the same place
 * virtua does, an effect that runs before the DOM update.
 */
vi.mock('virtua/svelte', async () => {
  const { default: List } = await import('./testing/List.svelte');
  return { VList: List };
});

// The column's record is frozen where it is handed out, so a write into any
// part of it - the prepend path's composed turns included - throws here too.
vi.mock('./conversation', async (importOriginal) => {
  const { frozenConversation } = await import('./testing/frozen');
  return frozenConversation(await importOriginal<typeof import('./conversation')>());
});

const { clear, firstGrowth, last, list } = await import('./testing/records');
const { default: Chat } = await import('./Chat.svelte');
const { installResizeObserver } = await import('./testing/viewport');

installResizeObserver();

const LEAD: SessionSlot = { org: 'Busytools', project: 'forge', label: 'lead' };

/** One turn as a page carries it. */
const turn = (key: string): unknown => ({
  key,
  messages: [{ type: 'user', uuid: `u-${key}`, message: { role: 'user', content: [] } }],
});

/**
 * A connection a test drives by hand, with a switch for whether it sends.
 *
 * `more` answering `false` is what a socket that is not open does, and the
 * chat is expected to have asked nothing when it happens.
 */
function stub() {
  const listeners = new Set<(message: ServerMessage) => void>();
  const asks: ClientMessage[] = [];
  let open = true;
  const connection = {
    subscribe: () => ({ state: () => ({ kind: 'ready' as const }) }),
    unsubscribe: () => undefined,
    refresh: () => undefined,
    dispatch: () => null,
    more: (conversation: SessionSlot, before: string | null, turns: number) => {
      if (!open) return false;
      asks.push({ kind: 'more', conversation, before, turns });
      return true;
    },
    onMessage: (fn: (message: ServerMessage) => void) => {
      listeners.add(fn);
      return () => listeners.delete(fn);
    },
    onStatus: () => () => undefined,
    store: () => undefined,
    settings: () => null,
    status: () => (open ? ('open' as const) : ('closed' as const)),
    close: () => undefined,
  } as unknown as Connection;

  return {
    connection,
    asks,
    shut: () => {
      open = false;
    },
    page(turns: unknown[], cursor: string | null): void {
      for (const fn of listeners) {
        fn({ kind: 'page', conversation: LEAD, turns, cursor });
      }
      flushSync();
    },
    /** One turn arriving below the reader, the way a live turn does. */
    append(): void {
      for (const fn of listeners) {
        fn({
          kind: 'update',
          update: {
            chat_appended: {
              key: LEAD,
              msg: { type: 'assistant', uuid: 'live', message: { content: [] } },
            },
          },
        });
      }
      flushSync();
    },
  };
}

let app: Record<string, unknown> | null = null;

function draw(server: ReturnType<typeof stub>): void {
  clear();
  app = mount(Chat, {
    target: document.body,
    props: { slot: LEAD, connection: server.connection },
  });
  flushSync();
}

/** Let the tick a settling compensation waits on run. */
async function tick(): Promise<void> {
  await new Promise((resolve) => setTimeout(resolve, 0));
  flushSync();
}

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  document.body.innerHTML = '';
});

beforeEach(() => {
  clear();
});

describe('the compensation the list is handed', () => {
  it('is armed before an older page arrives, and off again after it lands', async () => {
    const server = stub();
    draw(server);
    server.page([turn('t2')], '2');
    await tick();

    // The reader reaches the top, which asks for what is above. The
    // compensation has to be on for the rows that answer is about to add: it
    // is applied AS they change, so a page that armed it when the answer
    // arrived would be arming after the change it exists for.
    list()?.scrolledTo(0, 500, 320);
    flushSync();
    expect(server.asks, 'the ask went').toHaveLength(2);

    expect(last(), 'armed as the ask goes out').toEqual({ length: 1, shift: true });

    server.page([turn('t0'), turn('t2')], null);
    const grew = firstGrowth();
    expect(grew, 'the list gained a row').toBeDefined();
    expect(grew?.shift, 'with the compensation already on').toBe(true);

    await tick();
    expect(last()?.shift, 'and off again once the list has taken it').toBe(false);
  });

  it('is not armed by an ask the socket refused', async () => {
    const server = stub();
    draw(server);
    server.page([turn('t2')], '2');
    await tick();

    server.shut();
    list()?.scrolledTo(0, 500, 320);
    flushSync();

    // Nothing was sent, so no page is coming and there is nothing to
    // compensate for. Armed here, the flag stays on: the next turn appended
    // below the reader goes through the prepend path, which moves them AND
    // leaves the list's measured sizes attributed to the wrong rows.
    expect(last()?.shift, 'an ask that never went arms nothing').toBe(false);

    server.append();
    expect(last()?.shift, 'and the turn that arrives below it is not compensated').toBe(false);
  });

  it('is held while a second ask is still in flight', async () => {
    const server = stub();
    draw(server);
    server.page([turn('t2')], '2');
    await tick();

    list()?.scrolledTo(0, 500, 320);
    flushSync();
    server.page([turn('t1'), turn('t2')], '1');
    // The compensation is applied by scrolling the list, which fires a scroll
    // event of its own - so a second ask inside this window is the ordinary
    // case, not a contrived one.
    list()?.scrolledTo(0, 500, 320);
    flushSync();
    expect(server.asks, 'two asks are out').toHaveLength(3);
    expect(last()?.shift, 'still armed for the second').toBe(true);

    await tick();
    expect(last()?.shift, 'and one landing does not disarm the other').toBe(true);

    server.page([turn('t0'), turn('t1')], null);
    await tick();
    expect(last()?.shift, 'both landed, so off').toBe(false);
  });
});
