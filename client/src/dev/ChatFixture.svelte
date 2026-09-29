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
   * It is not a fallback. The page is loaded through a dynamic import behind
   * the DEV guard, so nothing ships that a page could draw in a server's
   * absence, and `fixture.test.ts` builds the app and fails if one does.
   */
  const SLOT: SessionSlot = { org: 'Busytools', project: 'forge', label: 'lead' };

  /**
   * A connection that answers the one page and never speaks again.
   *
   * It answers on the ASK rather than at subscribe, which is the order the
   * real one answers in and the order that makes the column draw its loading
   * state at all.
   */
  function answerWith(rows: unknown[]): Connection {
    let listening: ((message: ServerMessage) => void) | null = null;
    return {
      subscribe: () => ({ state: () => ({ kind: 'ready' as const }) }),
      unsubscribe: () => undefined,
      refresh: () => undefined,
      dispatch: () => null,
      more: () => {
        const page: ServerMessage = { kind: 'page', conversation: SLOT, rows, cursor: null };
        queueMicrotask(() => listening?.(page));
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
  }

  /** The canned page, or nothing outside a development build. */
  async function load(): Promise<unknown[] | null> {
    if (!import.meta.env.DEV) return null;
    const module = await import('./chat.fixture.json');
    return (module.default as { turns?: unknown[] }).turns ?? null;
  }

  const loaded = load();
</script>

{#await loaded then turns}
  {#if turns}
    {@const connection = answerWith(turns)}
    <!-- The column's own height, which in the session page is the grid's. -->
    <div class="devchat">
      <Chat slot={SLOT} {connection} cwd="/Users/vedhavyas/Projects/forge" />
    </div>
  {/if}
{/await}

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
</style>
