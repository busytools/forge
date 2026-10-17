// @vitest-environment jsdom
/**
 * The reader's thread card, mounted where the page mounts it - **and
 * re-mounted where CodeMirror re-mounts it**: a write changes the block key
 * and CM tears the card down and builds it again, so any state the card
 * keeps itself is state the reader loses. The resolved card's collapsed/
 * opened split is the worked case (the lead's gate, 2026-10-17): pressing
 * edit moves the reply slot, the key moves with it, and a locally-held
 * `opened` folded the thread back to its marker before its own edit box
 * could draw.
 */
import { flushSync, mount, unmount } from 'svelte';
import { SvelteSet } from 'svelte/reactivity';
import { describe, expect, it } from 'vitest';

import type { ReplyBox, TurnEditor, Voice } from '../review/boxes';
import type { ReviewThreadRecord } from '../session/wire';
import Inline from './Inline.svelte';
import type { RailRow } from './comments';

const THREAD: ReviewThreadRecord = {
  id: 't1',
  anchor: {
    path: 'src/lib.rs',
    side: 'new',
    line: 3,
    content_hash: '1',
    context: ['ctx'],
    base_ref: 'tree:Busytools/forge/lead',
  },
  comments: [
    {
      author: { kind: 'user' },
      text: 'settled in the end',
      at: '2026-01-01T00:00:00Z',
      review_id: null,
    },
  ],
  status: 'resolved',
  created_at: '2026-01-01T00:00:00Z',
  updated_at: '2026-01-01T00:00:00Z',
  commit: null,
  resolution: { state: 'placed', line: 3, moved_from: null },
};

const ROW: RailRow = { thread: THREAD, line: 3, movedFrom: null, stale: false };

/** The page's reply slot, which the edit rides too. */
function replyBox(): ReplyBox {
  return {
    thread: null,
    index: null,
    draft: '',
    busy: false,
    line: null,
    phase: null,
    silent: null,
    panel: false,
    take: null,
    wire: null,
  };
}

/** What dictation means to the page, stood in as the sheet's own type. */
const VOICE = {
  dictate: null,
  offered: false,
  connection: null,
  bind: 'right_cmd',
  mode: 'auto',
  axes: {},
  device: null,
  defaults: {},
  onaxes: () => undefined,
  ondevice: () => undefined,
} as unknown as Voice;

/** The page's edit box and its verbs, as Files.svelte wires them. */
function editorFor(box: ReplyBox): TurnEditor {
  return {
    box,
    voice: VOICE,
    begin: (thread, index, text) => {
      box.thread = thread;
      box.index = index;
      box.draft = text;
    },
    send: () => undefined,
    cancel: () => {
      box.thread = null;
      box.index = null;
      box.draft = '';
    },
    press: () => undefined,
    hold: () => undefined,
    field: () => undefined,
  };
}

/** Mount one card over the page's own state, as Files.svelte's blocks do -
 *  the opened set a `SvelteSet`, because the page's is and a plain one would
 *  not redraw the card at all (the trap this harness must not fall into). */
function draw(opened: SvelteSet<string>, reply: ReplyBox) {
  const host = document.createElement('div');
  document.body.append(host);
  const app = mount(Inline, {
    target: host,
    props: {
      row: ROW,
      rounds: [],
      now: Date.now(),
      busy: false,
      replying: false,
      replyBox: reply,
      voice: VOICE,
      editor: editorFor(reply),
      opened,
      onopen: (id: string) => opened.add(id),
      oncollapse: (id: string) => opened.delete(id),
      onrespond: () => undefined,
      onreply: () => undefined,
      onresolve: () => undefined,
      onreopen: () => undefined,
      ondelete: null,
      onpress: () => undefined,
      onhold: () => undefined,
    },
  });
  flushSync();
  return app;
}

describe('the reader card across a remount', () => {
  it('keeps a resolved thread open when CodeMirror rebuilds the card', async () => {
    // The page's own opened set: it outlives the card, which is the whole
    // point - CM tears the card down whenever a write moves the block key.
    const opened = new SvelteSet<string>();
    const reply = replyBox();

    const first = draw(opened, reply);
    const marker = document.querySelector<HTMLButtonElement>('.rv-marker');
    expect(marker, 'a resolved thread draws its marker').not.toBeNull();
    marker?.click();
    flushSync();
    expect(document.querySelector('.rv-thread'), 'pressing it opens the card').not.toBeNull();

    // What CM does when the reply slot moves: unmount, build again.
    await unmount(first);
    document.body.innerHTML = '';
    reply.thread = THREAD.id;
    reply.index = 0;
    reply.draft = 'the words';
    const second = draw(opened, reply);
    flushSync();

    expect(
      document.querySelector('.rv-thread'),
      'and the rebuilt card is still the open one',
    ).not.toBeNull();
    expect(
      document.querySelector('.rv-turnedit'),
      'with the edit box it was opened for',
    ).not.toBeNull();
    await unmount(second);
  });
});
