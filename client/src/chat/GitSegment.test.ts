import { render } from 'svelte/server';
import { afterEach, describe, expect, it } from 'vitest';

import GitSegment from './GitSegment.svelte';
import { git, vcsLine } from './git.svelte';

afterEach(() => {
  git.clear();
});

describe('the tree segment', () => {
  it('carries the tree glyph and the branch line', () => {
    git.sync({
      label: 'web-home-layout \u{b7} 3 files',
      head: "the project's tree",
      ahead: null,
      uncommitted: null,
      pr: null,
      gate: null,
    });
    const body = render(GitSegment, {}).body;

    expect(body, 'the glyph says what the segment is').toContain('i-git');
    expect(body, 'the branch and the count read on the toggle').toContain(
      'web-home-layout \u{b7} 3 files',
    );
  });

  it('draws nothing where the record has not landed', () => {
    git.sync(null);
    const body = render(GitSegment, {}).body;

    expect(body, 'no segment at all').not.toContain('sg-seg');
  });

  it('puts a repository event on the shown seat\'s strip as one transient line', () => {
    const slot = { org: 'TestOrg', project: 'forge', label: 'lead' };
    git.show(slot);
    git.sync({
      label: 'web-home-layout \u{b7} 3 files',
      head: "the project's tree",
      ahead: null,
      uncommitted: null,
      pr: null,
      gate: null,
    });
    git.note(slot, vcsLine('commit', 'main'));
    const body = render(GitSegment, {}).body;

    expect(body, 'the event rides beside the branch it moved').toContain('committed on main');
    expect(git.noteNow()?.line).toBe('committed on main');
  });

  it('names a mutation the build has no verb for, and a branchless one', () => {
    expect(vcsLine('checkout', 'topic')).toBe('repository changed (checkout) on topic');
    expect(vcsLine('merge', null)).toBe('merged');
    expect(vcsLine('push', 'main')).toBe('pushed main');
  });
});
