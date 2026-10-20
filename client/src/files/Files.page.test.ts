// @vitest-environment jsdom
/**
 * The Files page's own glyph wiring: the root row wears the theme's
 * root-folder glyph (its path is the lookup key when the manifest names a
 * root, and 5.39.0's root-name map is empty), a shut folder takes its shut
 * glyph and an open one its open glyph, and a file row takes its own. **The
 * seat's read is mocked**, as the review page's test mocks it: what this
 * file is about is what the page hands the tree, not the subscription that
 * carries the record, which `session/live.test.ts` covers over a real socket.
 */

import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

const seat = vi.hoisted(() => ({ listeners: new Set<(read: unknown) => void>() }));

vi.mock('../session/live', () => ({
  watchSession: () => ({
    subscribe: (fn: (read: unknown) => void) => {
      seat.listeners.add(fn);
      return () => {
        seat.listeners.delete(fn);
      };
    },
    hold: () => () => {},
  }),
}));

import type { Connection } from '../socket';
import type { SessionSlot } from '../wire/types';
import Files from './Files.svelte';

const SLOT: SessionSlot = { org: 'Busytools', project: 'forge', label: 'lead' };

/** Hand the page the record it would have folded out of a frame. */
function push(read: unknown): void {
  for (const fn of [...seat.listeners]) fn(read);
}

/** The seat's record, as the page reads it - only those fields are real. */
function record(scanCwd: string): unknown {
  return {
    state: { scan_cwd: scanCwd },
    file_index: {
      entries: {
        'src/main.rs': { rel_path: 'src/main.rs', depth: 1 },
        'crates/forge/lib.rs': { rel_path: 'crates/forge/lib.rs', depth: 2 },
        README: { rel_path: 'README', depth: 0 },
      },
    },
    tree_reviews: {
      threads: { outcome: 'read', value: [] },
      reviews: { outcome: 'read', value: [] },
    },
  };
}

let app: Record<string, unknown> | null = null;

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  document.body.innerHTML = '';
  seat.listeners.clear();
});

describe('the files page tree', () => {
  it('draws the root row from the scan path with the theme root folder', () => {
    app = mount(Files, {
      target: document.body,
      props: { connection: {} as Connection, slot: SLOT, dictation: false },
    });
    flushSync();
    push({ wire: record('/Users/me/proj'), refused: null });
    flushSync();

    const root = document.querySelector('.rv-root');
    expect(root, 'the root row drew').not.toBeNull();
    expect(root?.querySelector('.rv-rootname')?.textContent, 'named for the project').toBe('forge');
    expect(
      root?.querySelector('.rv-rootpath')?.getAttribute('title'),
      'the seat scanning path beside it',
    ).toBe('/Users/me/proj');
    expect(
      (root?.querySelector('img.rv-icon')?.getAttribute('src') ?? '').split('?')[0],
      'the theme root folder, keyed off the path',
    ).toMatch(/\/folder-root-open\.svg$/);

    const shut = document.querySelector('[data-path="src"] img.rv-icon');
    expect(
      (shut?.getAttribute('src') ?? '').split('?')[0],
      'a shut folder takes its shut glyph',
    ).toMatch(/\/folder-src\.svg$/);

    const open = document.querySelector('[data-path="crates"] img.rv-icon');
    expect(
      (open?.getAttribute('src') ?? '').split('?')[0],
      'a folder-only folder opens and takes its open glyph',
    ).toMatch(/\/folder-lib-open\.svg$/);

    const file = document.querySelector('[data-path="README"] img.rv-icon');
    expect(
      (file?.getAttribute('src') ?? '').split('?')[0],
      'and a file row takes its own glyph',
    ).toMatch(/\/readme\.svg$/);
  });
});
