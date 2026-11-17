// @vitest-environment jsdom
/**
 * The files page over a markdown file: a block press and its `+` carry the
 * page's own comment box, a thread draws under the block its landing falls
 * in, and the rail's jump scrolls to that block.
 *
 * **The two reads are stood in**: the seat's record and the file's bytes, so
 * what this file asks is what the page DRAWS of them, not how a socket
 * carries either (`session/live.test.ts` and `doc/live.test.ts` own that).
 */
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

const seat = vi.hoisted(() => ({ listeners: new Set<(read: unknown) => void>() }));
vi.mock('../session/live', () => ({
  watchSession: () => ({
    subscribe: (fn: (read: unknown) => void) => {
      seat.listeners.add(fn);
      return () => {
        seat.listeners.delete(fn);
      };
    },
  }),
}));

const doc = vi.hoisted((): { held: unknown } => ({ held: null }));
vi.mock('../doc/live', () => ({
  watchDocument: () => ({
    subscribe: (fn: (read: unknown) => void) => {
      fn(doc.held);
      return () => {};
    },
  }),
}));

import type { Connection } from '../socket';
import type { SessionSlot } from '../wire/types';
import Files from './Files.svelte';

const SLOT: SessionSlot = { org: 'Busytools', project: 'forge', label: 'lead' };

const TEXT = '---\ntitle: plan\n---\n# Heading\n\nBody text\n';

/** The seat-tree thread this page reads: placed on the blank line the
 *  heading's block owns, so it must draw under that block. */
const THREAD = {
  id: 't1',
  anchor: {
    path: 'plan.md',
    side: 'new',
    line: 5,
    content_hash: '1',
    context: ['# Heading'],
    base_ref: 'tree:Busytools/forge/lead',
  },
  comments: [{ author: { kind: 'user' }, text: 'the note', at: 'x', review_id: null }],
  status: 'open',
  created_at: 'x',
  updated_at: 'x',
  commit: null,
  resolution: { state: 'placed', line: 5, moved_from: null },
};

/** A thread written before the frontmatter was hidden: its line sits in no
 *  drawn row, so the first row carries its card. */
const HIDDEN = {
  ...THREAD,
  id: 't2',
  anchor: { ...THREAD.anchor, line: 2, context: ['title: plan'] },
  comments: [{ author: { kind: 'user' }, text: 'frontmatter note', at: 'x', review_id: null }],
  resolution: { state: 'placed', line: 2, moved_from: null },
};

function record(threads: unknown[], paths: string[] = ['plan.md']): unknown {
  return {
    state: { scan_cwd: '/Users/me/proj' },
    file_index: {
      entries: Object.fromEntries(paths.map((path) => [path, { rel_path: path, depth: 0 }])),
    },
    tree_reviews: {
      threads: { outcome: 'read', value: threads },
      reviews: { outcome: 'read', value: [] },
    },
  };
}

function push(read: unknown): void {
  for (const fn of [...seat.listeners]) fn(read);
}

function fakeConnection(): { sent: Record<string, unknown>[]; connection: Connection } {
  const sent: Record<string, unknown>[] = [];
  return {
    sent,
    connection: {
      settings: () => null,
      dispatch(command: unknown): null {
        sent.push(command as Record<string, unknown>);
        return null;
      },
      refresh: () => {},
      status: () => 'open',
      onStatus: () => () => {},
      onMessage: () => () => {},
      frame: () => true,
    } as unknown as Connection,
  };
}

let app: Record<string, unknown> | null = null;

beforeEach(() => {
  doc.held = {
    state: 'landed',
    path: 'plan.md',
    kind: 'markdown',
    bytes: new TextEncoder().encode(TEXT),
    reviewable: true,
    contentHash: '1',
    threads: [],
    reviews: [],
    refused: null,
  };
});

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  document.body.innerHTML = '';
  seat.listeners.clear();
});

/** The page with one markdown file open, over the threads it was given. */
function openFile(
  fake = fakeConnection(),
  threads: unknown[] = [THREAD],
): { sent: Record<string, unknown>[] } {
  app = mount(Files, {
    target: document.body,
    props: { connection: fake.connection, slot: SLOT, dictation: false },
  });
  flushSync();
  push({ wire: record(threads), refused: null });
  flushSync();
  document.querySelector<HTMLButtonElement>('[data-path="plan.md"]')?.click();
  flushSync();
  return { sent: fake.sent };
}

/** Type into the page's open comment box and file it. */
async function comment(words: string, sent: Record<string, unknown>[]): Promise<void> {
  const field = document.querySelector<HTMLTextAreaElement>('.files-block .rv-txarea');
  if (field === null) throw new Error('the comment box did not open');
  field.value = words;
  field.dispatchEvent(new Event('input', { bubbles: true }));
  flushSync();
  document.querySelector<HTMLButtonElement>('.files-block .rv-a.on')?.click();
  await vi.waitFor(() => {
    expect(
      sent.some((command) => command['upsert_review_thread'] !== undefined),
      'the comment reached the store',
    ).toBe(true);
  });
}

/** The anchor of the one upsert the page sent. */
function anchorOf(sent: Record<string, unknown>[]): {
  line: number;
  context: string[];
} {
  const upsert = sent.find((command) => command['upsert_review_thread'] !== undefined);
  if (upsert === undefined) throw new Error('no comment reached the store');
  const payload = upsert['upsert_review_thread'] as {
    thread: { anchor: { line: number; context: string[] } };
  };
  return payload.thread.anchor;
}

describe('the files page over markdown', () => {
  it('draws a block as the rendered markdown, with no source view', () => {
    openFile();

    expect(document.querySelector('.files-editor'), 'no CodeMirror for markdown').toBeNull();
    expect(document.querySelector('.doc-block h1')?.textContent).toBe('Heading');
    expect(document.body.textContent, 'the frontmatter is hidden').not.toContain('title: plan');
  });

  it("anchors a block press at the block's first line with the line's neighbours", async () => {
    const { sent } = openFile();

    document.querySelector<HTMLElement>('.doc-block[data-line="4"]')?.click();
    flushSync();
    await comment('a section note', sent);

    const anchor = anchorOf(sent);
    expect(anchor.line).toBe(4);
    // **The same neighbours the `+` captures.** The tree-scope resolver
    // relocates a moved thread only with the captured lines around the
    // anchor; a block's forward-only lines cannot.
    expect(anchor.context, 'the raw neighbours, frontmatter lines included').toEqual([
      '---',
      'title: plan',
      '---',
      '',
      'Body text',
    ]);
  });

  it("anchors the + at the same line with the line's neighbours", async () => {
    const { sent } = openFile();

    document.querySelector<HTMLButtonElement>('.doc-block[data-line="4"] .doc-add')?.click();
    flushSync();
    await comment('a line note', sent);

    const anchor = anchorOf(sent);
    expect(anchor.line).toBe(4);
    expect(anchor.context, 'the raw neighbours, frontmatter lines included').toEqual([
      '---',
      'title: plan',
      '---',
      '',
      'Body text',
    ]);
  });

  it('draws a thread under the block its landing falls in', () => {
    openFile();

    const block = document.querySelector<HTMLElement>('.doc-block[data-line="4"]');
    expect(
      block?.nextElementSibling?.querySelector('.rv-thread')?.textContent,
      'the card follows its own block',
    ).toContain('the note');
    expect(
      block?.nextElementSibling?.querySelector('.rv-thread')?.textContent,
      'and it is the landed thread',
    ).toContain('line 5');

    const later = document.querySelector<HTMLElement>('.doc-block[data-line="6"]');
    expect(
      later?.nextElementSibling?.querySelector('.rv-thread') ?? null,
      'the other block carries nothing',
    ).toBeNull();
  });

  /** A thread written before the frontmatter was hidden still has a card:
   *  its line sits in no drawn row, so the first row takes it rather than
   *  the reader losing a thread the rail still lists. */
  it('draws a thread anchored in the hidden frontmatter under the first block', () => {
    openFile(undefined, [HIDDEN]);

    const first = document.querySelector<HTMLElement>('.doc-block[data-line="4"]');
    expect(
      first?.nextElementSibling?.querySelector('.rv-thread')?.textContent,
      'the first row carries it',
    ).toContain('frontmatter note');
  });

  /** Every block is its own target: pressing a later row must anchor that
   *  row's line, not the first one's. */
  it("anchors a later block's press at its own line", async () => {
    const { sent } = openFile();
    document.querySelector<HTMLElement>('.doc-block[data-line="6"]')?.click();
    flushSync();
    await comment('a later note', sent);
    const anchor = anchorOf(sent);
    expect(anchor.line, 'the pressed block names its own line').toBe(6);
    expect(anchor.context, 'with line six\u{27}s own neighbours').toEqual(['---', '# Heading', '']);
  });

  it("anchors a later block's + at its own line", async () => {
    const { sent } = openFile();
    document.querySelector<HTMLButtonElement>('.doc-block[data-line="6"] .doc-add')?.click();
    flushSync();
    await comment('a later line note', sent);
    expect(anchorOf(sent).line, 'the + names the same line').toBe(6);
  });

  /** **The thread key carries the landing line.** A landing that moves
   *  INSIDE one block's span keeps the same row and the same `updated_at`,
   *  so without that segment the card is reused with its mount-time props
   *  and its header keeps naming the old line. */
  it('re-mounts a card when its landing moves inside the same block', () => {
    const at = { ...THREAD, resolution: { state: 'placed', line: 4, moved_from: null } };
    openFile(undefined, [at]);

    expect(
      document
        .querySelector('.doc-block[data-line="4"]')
        ?.nextElementSibling?.querySelector('.rv-thread')?.textContent,
      'the card starts on the first landing',
    ).toContain('line 4');

    push({
      wire: record([{ ...at, resolution: { state: 'placed', line: 5, moved_from: 4 } }]),
      refused: null,
    });
    flushSync();

    expect(
      document
        .querySelector('.doc-block[data-line="4"]')
        ?.nextElementSibling?.querySelector('.rv-thread')?.textContent,
      'the same block now draws the moved landing',
    ).toContain('line 5');
  });

  /** A resolved thread under a rendered block collapses to the shared
   *  marker, as it does under an editor line. */
  it('draws a resolved thread under its block as the shared marker', () => {
    openFile(undefined, [{ ...THREAD, status: 'resolved' }]);

    const marker = document
      .querySelector('.doc-block[data-line="4"]')
      ?.nextElementSibling?.querySelector('.rv-marker');
    expect(marker?.textContent, 'the marker names the state').toContain('resolved');
    expect(marker?.textContent, 'and where it landed').toContain('line 5');
  });

  /** **A landing past the frozen snapshot still draws.** The read is the
   *  snapshot it was opened at while a thread is placed against the CURRENT
   *  file, so a worker edit that grew the file can place a thread beyond the
   *  snapshot's last line; the editor path clamped it to the document's last
   *  line, and dropping it here would skip a row rule 25 forbids skipping. */
  it('draws a thread placed past the snapshot under the last block', () => {
    const past = {
      ...THREAD,
      id: 't3',
      anchor: { ...THREAD.anchor, line: 10 },
      resolution: { state: 'placed', line: 10, moved_from: 5 },
    };
    openFile(undefined, [past]);

    const last = [...document.querySelectorAll<HTMLElement>('.doc-block')].at(-1);
    expect(
      last?.nextElementSibling?.querySelector('.rv-thread')?.textContent,
      'the last row carries the out-of-snapshot card',
    ).toContain('line 10');
  });

  /** **The tree's arrows stop at a rendered block.** A block takes the
   *  keyboard, so a focused block's Arrow keys are the reader's - without
   *  the guard they step the tree and swap the open file under the reader. */
  it("keeps the tree's arrows out of a focused rendered block", () => {
    const fake = fakeConnection();
    app = mount(Files, {
      target: document.body,
      props: { connection: fake.connection, slot: SLOT, dictation: false },
    });
    flushSync();
    push({ wire: record([THREAD], ['plan.md', 'zzz.md']), refused: null });
    flushSync();
    document.querySelector<HTMLButtonElement>('[data-path="plan.md"]')?.click();
    flushSync();

    // The control: the arrow does step the tree from the page at large, so
    // a guard that simply swallowed every key would fail here.
    document.body.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowDown', bubbles: true }));
    flushSync();
    expect(document.querySelector('.files-path')?.textContent, 'the control steps the tree').toBe(
      'zzz.md',
    );

    document.querySelector<HTMLButtonElement>('[data-path="plan.md"]')?.click();
    flushSync();
    const block = document.querySelector<HTMLElement>('.doc-block[data-line="4"]');
    block?.focus();
    block?.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowDown', bubbles: true }));
    flushSync();

    expect(document.querySelector('.files-path')?.textContent, 'the block keeps the key').toBe(
      'plan.md',
    );
  });

  /** Run `press` with `scrollIntoView` captured, restoring the prototype
   *  afterwards so a later test never inherits the stub. */
  function withScrollCapture(press: () => void): HTMLElement | null {
    // The descriptor, not a torn-off method: jsdom defines no
    // `scrollIntoView` at all (measured on 30.1.1), so restoring a stored
    // `undefined` value would leave this stub installed for the rest of the
    // file. Put the descriptor back when there was one, delete otherwise.
    const original = Object.getOwnPropertyDescriptor(Element.prototype, 'scrollIntoView');
    let scrolled: HTMLElement | null = null;
    Element.prototype.scrollIntoView = function scrollIntoView(this: Element): void {
      scrolled = this as HTMLElement;
    };
    try {
      press();
    } finally {
      if (original !== undefined)
        Object.defineProperty(Element.prototype, 'scrollIntoView', original);
      else Reflect.deleteProperty(Element.prototype, 'scrollIntoView');
    }
    return scrolled;
  }

  it("scrolls to the block containing a rail row's line", () => {
    const scrolled = withScrollCapture(() => {
      openFile();
      document.querySelector<HTMLButtonElement>('.files-crow')?.click();
      flushSync();
    });

    expect(scrolled, 'the containing block is brought into view').not.toBeNull();
    expect(scrolled?.dataset.line).toBe('4');
    expect(Number(scrolled?.dataset.end)).toBeGreaterThanOrEqual(5);
  });

  it('scrolls to the first block for a thread anchored in the hidden frontmatter', () => {
    const scrolled = withScrollCapture(() => {
      openFile(undefined, [HIDDEN]);
      document.querySelector<HTMLButtonElement>('.files-crow')?.click();
      flushSync();
    });

    expect(scrolled?.dataset.line, 'the row its card draws under').toBe('4');
  });
});
