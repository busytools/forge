// @vitest-environment jsdom
/**
 * The files slip: **the one mandatory strip segment** (Ved, 2026-10-17) -
 * every seat has a tree, so it draws wherever a record does, and it is a
 * BARE DOOR: a press opens the files page, the label names the tree it
 * opens, and there is no count and no panel (the second and third rulings).
 *
 * The lead's gate, 2026-10-17: every sibling segment has a test and this
 * one had none.
 */
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it } from 'vitest';

import { files } from './files.svelte';
import FilesSegment from './FilesSegment.svelte';

let app: Record<string, unknown> | null = null;

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  document.body.innerHTML = '';
  files.sync(null, null);
});

function draw(href: string | null): void {
  app = mount(FilesSegment, { target: document.body, props: { href } });
  flushSync();
}

describe('the files slip', () => {
  it('draws nothing without a record', () => {
    draw(null);

    expect(document.body.querySelector('.sg-seg'), 'no tree, no slip').toBeNull();
  });

  it("is a plain door for the project's own checkout, with no count and no panel", () => {
    files.sync('/proj', null);
    draw(null);

    const label = document.body.querySelector('.sg-seg .sg-tog');
    expect(label?.textContent?.trim(), 'the label reads for the checkout').toBe('files');
    expect(label?.tagName.toLowerCase(), 'no href asks a span, not a door').toBe('span');
    expect(document.body.textContent, 'and no count rides it').not.toMatch(/\d/);
    expect(document.body.querySelector('.sg-list'), 'and there is no panel at all').toBeNull();
  });

  it('names the worktree it opens, and the door carries the href', () => {
    files.sync('/proj/.claude/worktrees/walk', 'walk');
    draw('/session/Busytools/forge/lead/files');

    const door = document.body.querySelector<HTMLAnchorElement>('a.sg-tog');
    expect(door?.textContent?.trim(), "the worktree's name, so the destination is no guess").toBe(
      'walk · files',
    );
    expect(door?.getAttribute('href')).toBe('/session/Busytools/forge/lead/files');
    expect(door?.getAttribute('aria-label')).toBe("the seat's files, in walk");
  });
});
