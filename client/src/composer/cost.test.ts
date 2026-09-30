// @vitest-environment jsdom
/**
 * What the composer re-derives for one keystroke and for one arriving frame.
 *
 * A count rather than a duration, because a duration on one machine says
 * nothing about the next one. The two axes are different questions and they
 * have different answers here: a keystroke owes the list it reopens, and a
 * frame owes nothing at all unless that list is open.
 *
 * The file index is the whole working tree, so the record carries one of a
 * realistic size - a seat measured in a browser held 2,286 entries. A fixture
 * with three files would make the per-frame rebuild look free, which is the
 * one thing this file exists to say it is not.
 */
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { record as blank, wire } from './testing';
import type { ComposerRecord } from './view';

vi.mock('./wire', async (importOriginal) => {
  const real = await importOriginal<typeof import('./wire')>();
  const { counting } = await import('../session/testing/counts');
  return {
    ...real,
    composerFrom: counting('composerFrom', real.composerFrom),
    askFrom: counting('askFrom', real.askFrom),
    advisoriesFrom: counting('advisoriesFrom', real.advisoriesFrom),
    agentTypesFrom: counting('agentTypesFrom', real.agentTypesFrom),
    filesFrom: counting('filesFrom', real.filesFrom),
  };
});

vi.mock('./view', async (importOriginal) => {
  const real = await importOriginal<typeof import('./view')>();
  const { counting } = await import('../session/testing/counts');
  return {
    ...real,
    composerState: counting('composerState', real.composerState),
    pendingAsk: counting('pendingAsk', real.pendingAsk),
    blocked: counting('blocked', real.blocked),
    noticeLine: counting('noticeLine', real.noticeLine),
  };
});

vi.mock('./autocomplete', async (importOriginal) => {
  const real = await importOriginal<typeof import('./autocomplete')>();
  const { counting } = await import('../session/testing/counts');
  return { ...real, offer: counting('offer', real.offer) };
});

const { default: Harness } = await import('./Harness.svelte');
const counts = await import('../session/testing/counts');

/** The size of a seat's file index, measured from a live one. */
const FILES = 2286;

/**
 * The seat's record: the shared fixture with the lists a real seat carries.
 *
 * The index is the point of this file, so it is the one field built rather
 * than defaulted - `filesFrom` is the builder whose per-frame cost the
 * composer used to pay.
 */
function seat(): ComposerRecord {
  const entries: Record<string, unknown> = {};
  for (let i = 0; i < FILES; i += 1) {
    const rel = `crates/forge-server/src/deep/dir/file_${i}.rs`;
    entries[rel] = {
      rel_path: rel,
      rel_path_lower: rel,
      basename_lower: `file_${i}.rs`,
      depth: 4,
    };
  }
  for (const rel of [
    'client/src/composer/view.ts',
    'client/src/composer/Composer.svelte',
    'client/src/composer/autocomplete.ts',
  ]) {
    entries[rel] = {
      rel_path: rel,
      rel_path_lower: rel,
      basename_lower: rel.split('/').pop() ?? rel,
      depth: 3,
    };
  }
  return blank({
    slash_commands: [
      { name: '/clear', description: 'Clear chat history' },
      { name: '/compact', description: 'Compact the conversation' },
    ],
    subagents: [{ name: 'Explore', description: 'Read-only search' }],
    file_index: { entries },
  });
}

/** The harness's own state, which a test sets the way a page would re-render it. */
interface Page {
  record: ComposerRecord;
}

let app: Record<string, unknown> | null = null;

function open(): void {
  app = mount(Harness, {
    target: document.body,
    props: { wire: wire(), initial: { record: seat() }, dictation: false },
  });
  flushSync();
}

/** What the reader types, which is the field's own event and not a prop. */
function type(text: string): void {
  const field = document.querySelector('textarea.txt');
  if (!(field instanceof HTMLTextAreaElement)) throw new Error('no field was drawn');
  field.value += text;
  field.dispatchEvent(new Event('input', { bubbles: true }));
  flushSync();
}

/** One arriving frame, which answers the page with a whole new record. */
function arrive(): void {
  const page = app?.['page'] as Page | undefined;
  if (page === undefined) throw new Error('the harness exposed no props');
  page.record = seat();
  flushSync();
}

const calls = (tally: { name: string; calls: number }[], name: string): number =>
  tally.find((row) => row.name === name)?.calls ?? 0;

beforeEach(() => {
  counts.clear();
});

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  document.body.innerHTML = '';
});

describe('what one keystroke costs', () => {
  /**
   * **The letter is the only thing a letter changes.** Everything here that
   * reads the record stayed put, so re-deriving any of it would be work the
   * reader's own act did not ask for - and the record is the expensive side of
   * this component, because the index in it is the whole working tree.
   */
  it('re-derives the list it reopens, and nothing that reads the record', () => {
    open();
    type('h');
    counts.clear();

    type('e');
    const tally = counts.tally();
    const measured = JSON.stringify(tally);

    expect(tally.length, `typing reached the component at all: ${measured}`).toBeGreaterThan(0);
    expect(calls(tally, 'offer'), measured).toBe(1);
    for (const name of [
      'composerState',
      'composerFrom',
      'pendingAsk',
      'askFrom',
      'blocked',
      'noticeLine',
      'filesFrom',
      'advisoriesFrom',
      'agentTypesFrom',
    ]) {
      expect(calls(tally, name), `${name} re-ran for a letter: ${measured}`).toBe(0);
    }
  });
});

describe('what one arriving frame costs', () => {
  /**
   * A frame replaces the record, and a draft with no trigger in it opens no
   * list - so the index the record carries is never read. This is the cost a
   * seat pays several times a second while a turn is in flight, and the one
   * the reader pays for without having asked for anything.
   */
  it('builds no list a plain draft would not open', () => {
    open();
    type('ship it');
    counts.clear();

    arrive();
    const tally = counts.tally();
    const measured = JSON.stringify(tally);

    // The frame landed: the component's own state was read again, so a zero
    // below is the lists not being asked for rather than a page standing still.
    expect(calls(tally, 'composerState'), `the frame reached the composer: ${measured}`).toBe(1);
    // Not even the offer, because a draft that opens no list is not a question
    // the record has any bearing on.
    expect(calls(tally, 'offer'), measured).toBe(0);
    for (const name of ['filesFrom', 'advisoriesFrom', 'agentTypesFrom']) {
      expect(
        calls(tally, name),
        `a frame built ${name} for a draft that opens nothing: ${measured}`,
      ).toBe(0);
    }
  });

  /**
   * The control the test above cannot be read without: the same counter, on the
   * same path, over a draft that DOES open a list. Without it a zero would be
   * indistinguishable from a builder nothing wrapped.
   */
  it('does build the index while a list is open, which is what makes the zero above mean anything', () => {
    open();
    type('@view');
    counts.clear();

    arrive();
    const tally = counts.tally();
    const measured = JSON.stringify(tally);

    expect(calls(tally, 'filesFrom'), measured).toBe(1);
    expect(calls(tally, 'offer'), measured).toBeGreaterThan(0);
  });
});
