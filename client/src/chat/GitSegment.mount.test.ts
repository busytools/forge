// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import GitSegment from './GitSegment.svelte';
import { git } from './git.svelte';
import type { GitStrip } from '../session/view';

/**
 * The segment's interaction state machine, which SSR cannot hold - the same
 * guards its siblings pin, because it is their sibling by construction: the
 * hover-into-the-gap crossing, the close grace, focus opening and Escape
 * closing (toggle and rows, with focus returned), the tap whose synthesised
 * enter must not eat its click, and the mouseup that lets a press go.
 *
 * On top of the machine it pins what is this row's own: the toggle carries no
 * panel when its list is empty (a clean tree on no pull request), so the row
 * states its branch without a door onto nothing.
 */
let app: Record<string, unknown> | null = null;

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  document.body.innerHTML = '';
  git.sync(null);
  vi.useRealTimers();
});

beforeEach(() => {
  vi.useFakeTimers();
});

const TREE: GitStrip = {
  label: 'web-home-layout \u{b7} 3 files',
  head: "the project's tree",
  ahead: {
    count: 2,
    base: 'main',
    commits: [
      {
        sha: 'a1b2c3d',
        subject: 'the first commit',
        stats: {
          files: [{ path: 'client/src/lib.rs', added: 12, removed: 4, status: 'modified' }],
          totalFiles: 1,
          totalAdded: 12,
          totalRemoved: 4,
        },
        time: Math.floor(Date.now() / 1000) - 7200,
      },
      {
        sha: 'd4e5f6a',
        subject: 'the second commit',
        stats: {
          files: [{ path: 'docs/new.md', added: 3, removed: 0, status: 'added' }],
          totalFiles: 1,
          totalAdded: 3,
          totalRemoved: 0,
        },
        time: Math.floor(Date.now() / 1000) - 3 * 86_400,
      },
    ],
    stats: {
      files: [
        { path: 'client/src/lib.rs', added: 12, removed: 4, status: 'modified' },
        { path: 'docs/new.md', added: 3, removed: 0, status: 'added' },
      ],
      totalFiles: 2,
      totalAdded: 15,
      totalRemoved: 4,
    },
  },
  uncommitted: {
    files: [
      { path: 'client/src/lib.rs', added: 12, removed: 4, status: 'modified' },
      { path: 'docs/new.md', added: 3, removed: 0, status: 'added' },
    ],
    totalFiles: 2,
    totalAdded: 15,
    totalRemoved: 4,
  },
  pr: { number: 1203, url: 'https://example.test/pull/1203', draft: false, closes: '#1200' },
  gate: null,
};

function draw(strip: GitStrip = TREE) {
  git.sync(strip);
  app = mount(GitSegment, { target: document.body });
  flushSync();
}

const toggle = () => document.querySelector<HTMLButtonElement>('.sg-tog');
const list = () => document.querySelector('.sg-list');
const rows = () => [...document.querySelectorAll<HTMLButtonElement>('.sg-it')];

/** A pointer event of the kind the browser synthesises, with its device said. */
function pointer(type: string, pointerType: string): PointerEvent {
  return new PointerEvent(type, { pointerType, bubbles: false });
}

describe("the tree row's interaction state machine", () => {
  it('reads a tap as open: the synthesised enter must not eat the click', () => {
    draw();
    toggle()?.dispatchEvent(pointer('pointerenter', 'touch'));
    toggle()?.click();
    flushSync();

    expect(list(), 'the first tap leaves the list open').not.toBeNull();
  });

  it('reads a tap as open when the press itself focuses the toggle first', () => {
    draw();
    window.dispatchEvent(pointer('pointerdown', 'touch'));
    window.dispatchEvent(pointer('pointerup', 'touch'));
    toggle()?.dispatchEvent(new MouseEvent('mousedown', { bubbles: true }));
    toggle()?.dispatchEvent(new FocusEvent('focusin', { bubbles: true }));
    toggle()?.dispatchEvent(new MouseEvent('mouseup', { bubbles: true }));
    toggle()?.click();
    flushSync();

    expect(list(), 'the tap opens and stays open').not.toBeNull();
  });

  it('opens on hover and closes after the grace when the pointer leaves', () => {
    draw();
    toggle()?.dispatchEvent(pointer('pointerenter', 'mouse'));
    flushSync();
    expect(list(), 'hover opens it').not.toBeNull();

    toggle()?.dispatchEvent(pointer('pointerleave', 'mouse'));
    flushSync();
    expect(list(), 'still open inside the grace').not.toBeNull();
    vi.advanceTimersByTime(150);
    flushSync();
    expect(list(), 'and closed after it').toBeNull();
  });

  it('opens on a keyboard focus after a press that focused nothing', () => {
    draw();
    toggle()?.dispatchEvent(new MouseEvent('mousedown', { bubbles: true }));
    toggle()?.dispatchEvent(new MouseEvent('mouseup', { bubbles: true }));
    flushSync();
    toggle()?.dispatchEvent(new FocusEvent('focusin', { bubbles: true }));
    flushSync();

    expect(list(), 'the keyboard focus still opens it').not.toBeNull();
  });

  it('opens on focus, and Escape closes it and puts focus back on the toggle', () => {
    draw();
    toggle()?.dispatchEvent(new FocusEvent('focusin', { bubbles: true }));
    flushSync();
    expect(list(), 'focus opens it').not.toBeNull();

    toggle()?.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }));
    flushSync();
    expect(list(), 'Escape closes it').toBeNull();
    expect(document.activeElement, 'and focus returns to the toggle').toBe(toggle());
  });

  it('keeps the list while focus moves inside it, and closes on a row Escape', () => {
    draw();
    toggle()?.click();
    flushSync();
    const row = rows()[0];
    expect(row, 'the list drew its row').not.toBeUndefined();
    if (row === undefined) return;

    row.focus();
    toggle()?.dispatchEvent(new FocusEvent('focusout', { bubbles: true, relatedTarget: row }));
    flushSync();
    vi.advanceTimersByTime(150);
    flushSync();
    expect(list(), 'focus inside keeps it').not.toBeNull();

    row.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }));
    flushSync();
    expect(list(), 'Escape on a row closes it').toBeNull();
    expect(document.activeElement, 'and focus returns to the toggle').toBe(toggle());
  });

  it('keeps the list when a pointer leave lands while a row holds focus', () => {
    // Opened by FOCUS rather than a tap, because the pointer really has left
    // the segment here - a click would leave jsdom matching `:hover`, where
    // the crossing guard shadows the row guard under test.
    draw();
    toggle()?.dispatchEvent(new FocusEvent('focusin', { bubbles: true }));
    flushSync();
    const row = rows()[0];
    row?.focus();
    toggle()?.dispatchEvent(pointer('pointerleave', 'mouse'));
    flushSync();
    vi.advanceTimersByTime(150);
    flushSync();

    expect(list(), 'the focused row keeps it').not.toBeNull();
  });

  it('closes when focus leaves the segment entirely', () => {
    draw();
    toggle()?.dispatchEvent(new FocusEvent('focusin', { bubbles: true }));
    flushSync();
    expect(list(), 'focus opened it').not.toBeNull();

    toggle()?.dispatchEvent(
      new FocusEvent('focusout', { bubbles: true, relatedTarget: document.body }),
    );
    flushSync();
    vi.advanceTimersByTime(150);
    flushSync();
    expect(list(), 'a focus that left the segment closes it').toBeNull();
  });

  it('closes a list the toggle opened when the toggle is clicked again', () => {
    draw();
    toggle()?.dispatchEvent(new FocusEvent('focusin', { bubbles: true }));
    flushSync();
    expect(list(), 'focus opened it, visibly').not.toBeNull();

    toggle()?.click();
    flushSync();

    expect(list(), 'the activation closes what the reader saw open').toBeNull();
  });

  it('draws the head, the chain, the files and the pull request', () => {
    draw();
    toggle()?.click();
    flushSync();

    const head = document.querySelector('.sg-head');
    expect(head?.textContent?.trim(), 'what the tree IS leads the hover').toBe(
      "the project's tree",
    );
    expect(
      [...document.querySelectorAll('.sg-head')].map((line) => line.textContent?.trim()),
      'each section states what it is',
    ).toEqual([
      "the project's tree",
      '2 commits ahead of main',
      '2 files \u{b7} +15 -4',
      'uncommitted \u{b7} 2 files \u{b7} +15 -4',
    ]);

    // The PR leads, the uncommitted rows follow and the chain closes: a
    // reader opens at the top and reads down from the present, and a long
    // chain cannot bury the PR below the scroll.
    expect(rows(), 'the pull request, two files and two commits').toHaveLength(5);
    const anchorRow = document.querySelector('a.sg-it');
    expect(
      (anchorRow?.compareDocumentPosition(rows()[1] as Node) ?? 0) &
        Node.DOCUMENT_POSITION_FOLLOWING,
      'the PR row did not lead the list',
    ).not.toBe(0);
    expect(
      rows()[1]?.querySelector('.fm')?.textContent?.trim(),
      'a file wears the mark its status maps to',
    ).toBe('M');
    expect(rows()[1]?.querySelector('.nm')?.textContent?.trim()).toBe('client/src/lib.rs');
    expect(rows()[1]?.querySelector('.n')?.textContent?.trim()).toBe('+12 -4');
    expect(rows()[2]?.querySelector('.fm')?.textContent?.trim()).toBe('A');
    expect(
      rows()[3]?.querySelector('.sha')?.textContent?.trim(),
      'the chain follows, newest first, each with its sha',
    ).toBe('a1b2c3d');
    expect(rows()[3]?.querySelector('.nm')?.textContent?.trim()).toBe('the first commit');
    expect(rows()[3]?.querySelector('.n')?.textContent?.trim(), 'and when it landed').toBe('2h');
    expect(rows()[4]?.querySelector('.sha')?.textContent?.trim()).toBe('d4e5f6a');
    expect(rows()[4]?.querySelector('.n')?.textContent?.trim()).toBe('3d');
    expect(
      document.querySelector('a.sg-it')?.textContent?.replace(/\s+/g, ' ').trim(),
      'the pull request states its own number and state',
    ).toBe('PR #1203 open');

    const anchor = document.querySelector('a.sg-it');
    expect(anchor?.getAttribute('href'), 'and is a link, because the PR is a place').toBe(
      'https://example.test/pull/1203',
    );
    expect(document.querySelector('.sg-sub')?.textContent?.trim()).toBe('closes #1200');

    // A file row has no destination yet, so picking it closes the list; a
    // commit row reveals instead, pinned in its own test above.
    rows()[1]?.click();
    flushSync();
    expect(list(), 'the pick closes the list').toBeNull();
  });

  /**
   * **A commit's own files are depth.** The chain says what the branch ran
   * as; what each commit CHANGED is on demand under its row - on hover,
   * on focus, and on a tap, since a touch has no hover to give.
   */
  it("reveals a commit's own files on hover, focus and tap", () => {
    draw();
    toggle()?.click();
    flushSync();

    const reveal = () => document.querySelector('.sg-cm');
    const revealed = () => reveal()?.querySelector('.nm.path')?.textContent?.trim();
    // The commit rows, told from the file rows a reveal draws beside them.
    const commits = (): HTMLButtonElement[] =>
      rows().filter((row) => row.querySelector('.sha') !== null);
    expect(reveal(), 'quiet until a commit row is pointed at').toBeNull();

    // The hover door.
    commits()[0]?.dispatchEvent(pointer('pointerenter', 'mouse'));
    flushSync();
    expect(revealed(), "the hover shows the commit's own files").toBe('client/src/lib.rs');
    expect(
      (commits()[0]?.compareDocumentPosition(reveal() as Node) ?? 0) &
        Node.DOCUMENT_POSITION_FOLLOWING,
      'the reveal sits under the row it belongs to',
    ).not.toBe(0);

    // The tap door, on the other commit: the reveal follows the row.
    commits()[1]?.click();
    flushSync();
    expect(revealed(), "the tap shows that commit's own files").toBe('docs/new.md');

    // The focus door.
    commits()[0]?.focus();
    flushSync();
    expect(revealed(), 'the focus walks the reveal back').toBe('client/src/lib.rs');
  });

  /**
   * **A tree that could not be read says why.** Every other fixture here
   * carries `gate: null`, so without this one a seat whose git could not be
   * read would silently lose the only line that says so.
   */
  it('draws the gate line when the tree could not be read', () => {
    draw({
      label: 'no branch',
      head: "the project's tree",
      ahead: null,
      uncommitted: null,
      pr: null,
      gate: 'its working directory is not there',
    });
    toggle()?.click();
    flushSync();

    expect(
      rows().at(-1)?.querySelector('.nm')?.textContent?.trim(),
      "the gate line is the panel's last word",
    ).toBe('its working directory is not there');
    expect(
      [...document.querySelectorAll('.sg-head')].map((line) => line.textContent?.trim()),
      'and the tree is not claimed clean beside it',
    ).toEqual(["the project's tree"]);
  });

  /**
   * A tree with nothing else to state still opens onto its head: whose
   * tree this is is itself a fact, and a toggle holding a door onto
   * nothing would be the one case the panel has no use for.
   */
  it('opens onto the head alone when there is nothing else to state', () => {
    draw({
      label: 'feat/x',
      head: "a worker's tree",
      ahead: null,
      uncommitted: null,
      pr: null,
      gate: null,
    });
    toggle()?.click();
    flushSync();

    expect(list(), 'the panel opened').not.toBeNull();
    expect(document.querySelector('.sg-head')?.textContent?.trim()).toBe("a worker's tree");
    expect(
      [...document.querySelectorAll('.sg-head')].map((line) => line.textContent?.trim()),
      'and states the tree is clean',
    ).toEqual(["a worker's tree", 'uncommitted \u{b7} clean']);
    expect(rows(), 'and holds no rows beyond it').toHaveLength(0);
  });
});
