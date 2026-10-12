// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import session from '../dev/fixtures/session.json';
import { reviewRows } from '../session/view';
import { diffFrom, gitFrom, reviewsFrom, sessionFrom, type SessionRecord } from '../session/wire';
import ReviewSegment from './ReviewSegment.svelte';
import { review } from './review.svelte';

/**
 * The review row's draw condition, at the component: the door to the review
 * page stands only while the page has something behind it. Ved reported the
 * row drawing on 1.1.9 with nothing on the seat. The store is fed through
 * the page's own derivation here, so the strip and the page cannot drift.
 */
const record: SessionRecord = sessionFrom(session);

/** The fixture's record with its branch-ahead layer stated, so a case can
 *  take the seat's own changes away. */
const withBranch = (branchAhead: unknown): SessionRecord => ({
  ...record,
  diff: diffFrom({ worktree: 'clean', branch_ahead: branchAhead, truncated: false }),
  git: gitFrom({ default_branch: 'main', worktree: null, ahead: null }),
});

/** One thread on committed code, answered by an agent: something to read. */
const withThreads = (held: SessionRecord): SessionRecord => ({
  ...held,
  reviews: reviewsFrom({
    threads: {
      outcome: 'read',
      value: [
        {
          id: 'answered',
          anchor: {
            path: 'src/a.rs',
            side: 'New',
            line: 7,
            content_hash: '1',
            context: [],
            base_ref: 'main',
          },
          comments: [
            {
              author: { Agent: { label: 'w1' } },
              text: 'answered words',
              at: '2026-01-01T00:00:00Z',
            },
          ],
          status: 'Addressed',
          created_at: '2026-01-01T00:00:00Z',
          updated_at: '2026-01-01T00:00:00Z',
        },
      ],
    },
    reviews: { outcome: 'read', value: [] },
  }),
});

let app: Record<string, unknown> | null = null;

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  document.body.innerHTML = '';
  review.sync(null);
  vi.useRealTimers();
});

beforeEach(() => {
  vi.useFakeTimers();
});

function draw(held: SessionRecord, href: string | null = null): void {
  review.sync(reviewRows(held));
  app = mount(ReviewSegment, { target: document.body, props: { href } });
  flushSync();
}

const toggle = () => document.querySelector<HTMLButtonElement>('.sg-tog');

describe("the review row's draw", () => {
  it('draws no row on a seat with nothing under review', () => {
    draw(withBranch('clean'));
    expect(toggle(), 'a clean branch with nothing filed has nothing behind the door').toBeNull();
  });

  it('keeps the door while the branch carries changes of its own', () => {
    draw(record);
    expect(toggle(), 'the fixture branch carries a commit ahead').not.toBeNull();
    expect(toggle()?.textContent, 'nothing filed reads as the door itself').toContain('review');
  });

  it('keeps the door while a thread sits on a clean branch', () => {
    draw(withThreads(withBranch('clean')));
    expect(toggle(), 'threads on committed code are still something to review').not.toBeNull();
    expect(toggle()?.textContent, 'and the row leads with what waits').toContain('1 to read');
  });

  it('draws nothing for a read that never landed', () => {
    review.sync(null);
    app = mount(ReviewSegment, { target: document.body });
    flushSync();
    expect(toggle(), 'no read, no row').toBeNull();
  });
});

describe("the review panel's placement", () => {
  it('portals its panel to the body, where the viewport terms land', () => {
    // The panel's style is measured in viewport terms (`strip-panel.ts`), so
    // a panel left inside the segment - its `offsetParent` - lands hundreds
    // of pixels from the toggle (measured on the shallow look: the panel drew
    // at x=-128 under a segment at x=518). Every sibling segment portals for
    // the same reason.
    draw(record);
    toggle()?.click();
    flushSync();
    const list = document.querySelector('.sg-list');
    expect(list, 'the click opened the panel').not.toBeNull();
    expect(
      document.body.querySelector(':scope > .sg-list'),
      'the panel lives in the body, not inside the segment',
    ).toBe(list);
  });

  it('keeps the panel while focus moves into it', () => {
    // The portaled panel is outside the segment, so focus entering it is not
    // a departure - the guard reads the panel as well as the segment.
    draw(record, '/session/Busytools/forge/lead/review');
    toggle()?.dispatchEvent(new FocusEvent('focusin', { bubbles: true }));
    flushSync();
    const door = document.querySelector<HTMLAnchorElement>('.sg-open');
    expect(door, 'the panel drew its door').not.toBeNull();
    door?.focus();
    toggle()?.dispatchEvent(new FocusEvent('focusout', { bubbles: true, relatedTarget: door }));
    flushSync();
    vi.advanceTimersByTime(150);
    flushSync();
    expect(document.querySelector('.sg-list'), 'focus inside keeps it').not.toBeNull();
  });
});
