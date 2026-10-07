import { render } from 'svelte/server';
import { afterEach, describe, expect, it } from 'vitest';

import GitSegment from './GitSegment.svelte';
import { git } from './git.svelte';

afterEach(() => {
  git.sync(null);
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
});
