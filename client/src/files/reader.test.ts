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
const bridge = vi.hoisted(() => ({ langs: [] as string[] }));
vi.mock('@cmshiki/shiki', () => ({
  shikiToCodeMirror: (options: { lang: string }): Promise<{ shiki: unknown[] }> => {
    bridge.langs.push(options.lang);
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
});

describe('the reader built over a file', () => {
  it('asks the bridge for the language its path names, and for text when none does', async () => {
    const rust = document.createElement('div');
    document.body.append(rust);
    const built = await buildReader(rust, 'fn main() {}', 'crates/a.rs');
    built.destroy();

    const plain = document.createElement('div');
    document.body.append(plain);
    const other = await buildReader(plain, 'fn main() {}', 'notes.zzz');
    other.destroy();

    expect(bridge.langs, 'languageForPath reaches the reader, per path').toEqual(['rust', 'text']);
    expect(built.lines(), 'and the handle carries the file\u{27}s own lines').toEqual([
      'fn main() {}',
    ]);
  });

  it('draws the target line with its door, and mounts a block under one', async () => {
    const host = document.createElement('div');
    document.body.append(host);
    const reader = await buildReader(host, 'one\ntwo\nthree', 'a.rs');

    reader.targetLine(2);
    expect(host.querySelector('.cm-line.files-target')?.textContent, 'the line is tinted').toBe(
      'two',
    );
    expect(host.querySelector('.files-door'), 'and the door rides the gutter').not.toBeNull();

    const pressed: number[] = [];
    reader.destroy();

    const second = document.createElement('div');
    document.body.append(second);
    const withComment = await buildReader(second, 'one\ntwo', 'a.rs', (line) => pressed.push(line));
    const line = second.querySelector('.cm-line');
    line?.dispatchEvent(new MouseEvent('mousedown', { bubbles: true }));
    expect(pressed, 'a press on a line reports it').toEqual([1]);
    withComment.destroy();
  });

  it('mounts a thread block into its row', async () => {
    const host = document.createElement('div');
    document.body.append(host);
    const reader = await buildReader(host, 'one\ntwo', 'a.rs');

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

  function draw(value: DocRead): void {
    reads.held = value;
    app = mount(Reader, {
      target: document.body,
      props: {
        connection: {} as never,
        slot: SLOT,
        path: '/proj/a.rs',
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

  it('draws a kind it has no text for as its own line', () => {
    draw(answer({ kind: 'pdf', bytes: new Uint8Array([1, 2, 3]), reviewable: false }));

    expect(document.querySelector('.files-quiet')?.textContent).toContain('draws no text');
  });
});
