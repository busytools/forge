<script lang="ts">
  import Chat from '../chat/Chat.svelte';
  import type { ServerMessage } from '../protocol';
  import type { Connection } from '../socket';
  import type { SessionSlot } from '../wire/types';

  /**
   * The conversation drawn from a canned page, reached only in a development
   * build.
   *
   * **It exists so the chat can be looked at without a running forge**, which
   * is otherwise impossible while nothing mounts it: the session page's column
   * belongs to the shell, and a seat's own history needs a live server.
   *
   * **It answers, and it can be made to append.** The two things a page's
   * scroll behaviour turns on are a turn arriving BELOW the reader and older
   * turns arriving ABOVE them, and neither can be reached from a fixture that
   * answers once and stops: a canned page holds still, so a page that scrolled
   * its reader on every frame would look exactly like one that never moved.
   * The two buttons under the column are the instrument for that, and they are
   * dev-only chrome like the rest of this file.
   *
   * It is not a fallback. The page is loaded through a dynamic import behind
   * the DEV guard, so nothing ships that a page could draw in a server's
   * absence, and `fixture.test.ts` builds the app and fails if one does.
   */

  /** The canned page, and the seat it is answered for. */
  interface Canned {
    slot: SessionSlot;
    turns: unknown[];
    /** One older turn, answered when the page is asked for what is above it. */
    older: unknown;
  }

  /**
   * A connection that answers the canned page, and can be told to append.
   *
   * It answers on the ASK rather than at subscribe, which is the order the
   * real one answers in and the order that makes the column draw its loading
   * state at all.
   */
  function answerWith(canned: Canned) {
    let listening: ((message: ServerMessage) => void) | null = null;
    const say = (message: ServerMessage): void => {
      queueMicrotask(() => listening?.(message));
    };

    const connection = {
      subscribe: () => ({ state: () => ({ kind: 'ready' as const }) }),
      unsubscribe: () => undefined,
      refresh: () => undefined,
      dispatch: () => null,
      more: (_slot: SessionSlot, before: string | null) => {
        // A cursor asks for what is above what is held, which is the prepend
        // the reader's place has to survive. No cursor is the newest page.
        const turns = before === null ? canned.turns : [canned.older];
        say({
          kind: 'page',
          conversation: canned.slot,
          turns,
          cursor: before === null ? '1' : null,
        });
        return true;
      },
      onMessage: (fn: (message: ServerMessage) => void) => {
        listening = fn;
        return () => {
          listening = null;
        };
      },
      onStatus: () => () => undefined,
      store: () => undefined,
      settings: () => null,
      status: () => 'open' as const,
      close: () => undefined,
    } as unknown as Connection;

    let nth = 0;

    /** One turn arriving below the reader, the way a live turn does. */
    function append(): number {
      nth += 1;
      say({
        kind: 'update',
        update: {
          chat_appended: {
            key: canned.slot,
            msg: {
              type: 'assistant',
              uuid: `appended-${nth}`,
              message: {
                id: `msg_appended_${nth}`,
                role: 'assistant',
                model: 'claude-opus-5',
                content: [
                  { type: 'text', text: `A line that arrived while you were reading (${nth}).` },
                ],
              },
            },
          },
        },
      });
      return nth;
    }

    /**
     * Append on a timer, which is what makes the scroll behaviour observable
     * without a hand on the mouse: a reader parked mid-column cannot be
     * watching for a button, and what the page has to survive is a turn
     * arriving while they are reading something else.
     */
    function every(ms: number): () => void {
      const timer = setInterval(append, ms);
      return () => clearInterval(timer);
    }

    return { connection, append, every };
  }

  /**
   * The canned page, or nothing outside a development build.
   *
   * **The connection is built ONCE, outside the template.** Built inside it, a
   * re-render hands the column a new connection object, and the column's own
   * effect keys on that object's identity: it tears the conversation down and
   * starts it again, which is a page that empties itself for no reason a reader
   * could name.
   */
  const canned = $state<{
    page: Canned | null;
    held: ReturnType<typeof answerWith> | null;
    stop: (() => void) | null;
  }>({ page: null, held: null, stop: null });

  if (import.meta.env.DEV) {
    void import('./chat.fixture.json').then((module) => {
      const page = module.default as Canned;
      if (page.turns === undefined) return;
      canned.page = page;
      canned.held = answerWith(page);
      // Every four seconds a turn lands below the reader, which is the case
      // the column's scroll behaviour is for.
      canned.stop = canned.held.every(4000);
    });
  }
</script>

{#if canned.page !== null && canned.held !== null}
  <!-- The column's own height, which in the session page is the grid's. -->
  <div class="devchat">
    <Chat
      slot={canned.page.slot}
      connection={canned.held.connection}
      cwd="/Users/vedhavyas/Projects/forge"
    />
  </div>
  <div class="devbar">
    <button onclick={canned.held.append}>append a turn</button>
    <span>a turn lands every four seconds; scroll the column and watch where it stays</span>
  </div>
{/if}

<style>
  /* A development harness rather than a page: the column is given the height
     the session page's grid gives it, so the list is looked at at the size it
     really draws in. */
  .devchat {
    display: flex;
    flex-direction: column;
    height: 78vh;
    margin-top: 24px;
  }

  .devbar {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 8px 0;
    font-family: var(--mono);
    font-size: var(--fs-data);
    color: var(--dim);
  }
</style>
