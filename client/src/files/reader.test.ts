// @vitest-environment jsdom
/**
 * The reader, built for real: `buildReader` pulls CodeMirror and shiki, so
 * these cases run the machinery the page actually draws - the language a
 * path reaches, the line a press targets and the door beside it, a thread
 * block mounted into a row, and the states the component owes around them.
 *
 * The lead's gate, 2026-10-17: the plan named this file and it was never
 * written, so nothing pinned the reader's own behaviour.
 */
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

const reads = vi.hoisted((): { held: unknown } => ({ held: null }));

/** The page's read, stood in: this file asks what the reader DRAWS of an
 *  answer, not how the socket carries one (`doc/live.test.ts` owns that). */
vi.mock('../doc/live', () => ({
  watchDocument: () => ({
    subscribe: (fn: (value: unknown) => void) => {
      fn(reads.held);
      return () => {};
    },
  }),
}));

/**
 * **The colour bridge is stood in for its CALL, not its painting.** Under
 * vitest the bridge hands the editor a `Compartment` from its own resolved
 * `@codemirror/state` - two instances, CM6's "Unrecognized extension value"
 * - and `resolve.dedupe`, inlining and test-mode aliases were each measured
 * unable to reach across the node pipeline (2026-10-17). What is OURS to
 * pin is which grammar a path asks for and that the rest of the reader
 * builds; the painting itself is the bridge's, verified in the browser.
 */
const bridge = vi.hoisted(() => ({
  langs: [] as string[],
  /** Set to make the bridge fail, the way a rejected grammar chunk does. */
  fail: null as Error | null,
}));
vi.mock('@cmshiki/shiki', () => ({
  shikiToCodeMirror: (options: { lang: string }): Promise<{ shiki: unknown[] }> => {
    bridge.langs.push(options.lang);
    if (bridge.fail !== null) return Promise.reject(bridge.fail);
    return Promise.resolve({ shiki: [] });
  },
}));

import type { DocRead } from '../doc/live';
import type { SessionSlot } from '../wire/types';
import Reader from './Reader.svelte';
import { buildReader } from './reader';

const SLOT: SessionSlot = { org: 'Busytools', project: 'forge', label: 'lead' };

function answer(overrides: Partial<DocRead>): DocRead {
  return {
    state: 'landed',
    path: '/proj/a.rs',
    kind: { other: 'rs' },
    bytes: new TextEncoder().encode('fn main() {}'),
    reviewable: true,
    contentHash: '1',
    threads: [],
    reviews: [],
    refused: null,
    ...overrides,
  };
}

beforeEach(() => {
  reads.held = answer({});
  bridge.langs = [];
  bridge.fail = null;
  // jsdom has no object-URL implementation, and the picture and the PDF both
  // draw from the read's bytes through one.
  URL.createObjectURL = vi.fn(() => 'blob:picture');
  URL.revokeObjectURL = vi.fn();
});

describe('the reader built over a file', () => {
  it('asks the bridge for the language it is handed, and for text when handed text', async () => {
    const rust = document.createElement('div');
    document.body.append(rust);
    const built = await buildReader(rust, 'fn main() {}', 'rust');
    built.destroy();

    const plain = document.createElement('div');
    document.body.append(plain);
    const other = await buildReader(plain, 'fn main() {}', 'text');
    other.destroy();

    expect(bridge.langs, 'the policy reaches the reader, per path').toEqual(['rust', 'text']);
    expect(built.lines(), 'and the handle carries the file\u{27}s own lines').toEqual([
      'fn main() {}',
    ]);
  });

  it('draws the target line with its door, and mounts a block under one', async () => {
    const host = document.createElement('div');
    document.body.append(host);
    const reader = await buildReader(host, 'one\ntwo\nthree', 'rust');

    reader.targetLine(2);
    expect(host.querySelector('.cm-line.files-target')?.textContent, 'the line is tinted').toBe(
      'two',
    );
    expect(host.querySelector('.files-door'), 'and the door rides the gutter').not.toBeNull();

    const pressed: number[] = [];
    reader.destroy();

    const second = document.createElement('div');
    document.body.append(second);
    const withComment = await buildReader(second, 'one\ntwo', 'rust', (line) => pressed.push(line));
    const line = second.querySelector('.cm-line');
    line?.dispatchEvent(new MouseEvent('mousedown', { bubbles: true }));
    expect(pressed, 'a press on a line reports it').toEqual([1]);
    withComment.destroy();
  });

  it('mounts a thread block into its row', async () => {
    const host = document.createElement('div');
    document.body.append(host);
    const reader = await buildReader(host, 'one\ntwo', 'rust');

    reader.setBlocks([
      {
        line: 2,
        key: 'thread:t1',
        mount: (into) => {
          into.textContent = 'the thread card';
          return () => {};
        },
      },
    ]);
    expect(host.textContent, 'the block drew under its line').toContain('the thread card');
    reader.destroy();
  });
});

describe('the reader component around it', () => {
  let app: Record<string, unknown> | null = null;
  afterEach(async () => {
    if (app !== null) await unmount(app);
    app = null;
    document.body.innerHTML = '';
  });

  function draw(value: DocRead, path = '/proj/a.rs'): void {
    reads.held = value;
    app = mount(Reader, {
      target: document.body,
      props: {
        connection: {} as never,
        slot: SLOT,
        path,
        handle: null,
      },
    });
    flushSync();
  }

  it('draws the read pending, as itself', () => {
    draw(answer({ state: 'reading', bytes: null }));

    expect(document.body.textContent).toContain('Reading /proj/a.rs...');
  });

  it("draws the server's refusal in its own words", () => {
    draw(
      answer({ state: 'refused', bytes: null, refused: 'the file is 24 MiB, over the ceiling' }),
    );

    expect(document.querySelector('.files-quiet')?.textContent).toContain('over the ceiling');
  });

  it('opens an extensionless text file, with the header saying why it is plain', () => {
    draw(
      answer({
        path: '/proj/notes.weird',
        kind: 'text',
        bytes: new TextEncoder().encode('nothing here matches a grammar'),
      }),
      '/proj/notes.weird',
    );

    expect(document.querySelector('.files-fact')?.textContent).toBe(
      'plain text - no syntax for this file',
    );
    expect(document.querySelector('.files-hint'), 'and it opens in the editor').not.toBeNull();
    expect(document.querySelector('.files-quiet'), 'not a refusal').toBeNull();
  });

  it('opens a Justfile coloured with its own grammar', () => {
    draw(
      answer({
        path: '/proj/Justfile',
        kind: 'text',
        bytes: new TextEncoder().encode('check:\n  cargo fmt'),
      }),
      '/proj/Justfile',
    );

    expect(document.querySelector('.files-fact')?.textContent).toContain('just');
  });

  /**
   * **The head and the body read the same bytes.** An extensionless script
   * names its language only by its shebang, and a body handed a path alone
   * would draw it plain while the head said `shellscript`.
   */
  it('reads an extensionless script shebang into the head and the editor', async () => {
    draw(
      answer({
        path: '/proj/run',
        kind: 'text',
        bytes: new TextEncoder().encode('#!/usr/bin/env bash\necho hi\n'),
      }),
      '/proj/run',
    );

    expect(document.querySelector('.files-fact')?.textContent).toContain('shellscript');
    await vi.waitFor(() => {
      flushSync();
      expect(bridge.langs, 'the editor is built with the shebang grammar').toContain('shellscript');
    });
  });

  /**
   * **A reader that could not be built is a state, not an empty editor.**
   * A failed chunk fetch or a rejected grammar used to leave the box blank
   * with nothing saying why.
   */
  it('draws a failed reader build instead of an empty editor', async () => {
    bridge.fail = new Error('the grammar chunk did not arrive');
    draw(answer({}));

    await vi.waitFor(() => {
      flushSync();
      expect(document.querySelector('.files-quiet')?.textContent ?? '').toContain(
        'could not be built',
      );
    });
    expect(document.querySelector('.files-quiet')?.textContent).toContain('did not arrive');
  });

  it('draws a picture inline with its mime in the header', () => {
    draw(
      answer({
        path: '/proj/mark.png',
        kind: { other: 'png' },
        bytes: new Uint8Array([0x89, 0x50, 0x4e, 0x47]),
        reviewable: false,
      }),
      '/proj/mark.png',
    );

    const picture = document.querySelector<HTMLImageElement>('.files-media img');
    expect(picture, 'the picture itself').not.toBeNull();
    expect(picture?.src, 'drawn from the read bytes').toBe('blob:picture');
    expect(document.querySelector('.files-fact')?.textContent).toContain('image/png');
  });

  it('draws a PDF as its pages', () => {
    draw(
      answer({
        path: '/proj/retro.pdf',
        kind: 'pdf',
        bytes: new TextEncoder().encode('%PDF-1.4 fake'),
        reviewable: false,
      }),
      '/proj/retro.pdf',
    );

    expect(document.querySelector('embed.doc-pdf'), 'the page viewer').not.toBeNull();
    expect(document.querySelector('.files-fact')?.textContent).toContain('application/pdf');
  });

  it('draws a notice naming the type and size when there is no viewer', () => {
    draw(
      answer({
        path: '/proj/forge.tar.gz',
        kind: { other: 'gz' },
        bytes: new Uint8Array([0xff, 0xfe, 0x00, 0x01]),
        reviewable: false,
      }),
      '/proj/forge.tar.gz',
    );

    const notice = document.querySelector('.files-notice')?.textContent ?? '';
    expect(notice).toContain('application/gzip');
    expect(notice).toContain('not text');
    expect(notice).toContain('still in the tree');
    expect(document.querySelector('.files-fact')?.textContent, 'the head names it too').toContain(
      'archive',
    );
  });
});
