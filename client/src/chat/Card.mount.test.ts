// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it } from 'vitest';

import Card from './Card.svelte';

let app: Record<string, unknown> | null = null;

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  document.body.innerHTML = '';
});

describe('a question card', () => {
  it('keeps the question glyph and names an omitted record on the marks line', () => {
    // The served page caps the answers record (#2374). The card must still be
    // the question's own drawing - its glyph and the words it asked - with the
    // size on the diff's marks line, not a plain tool row and a body of its
    // own.
    const host = document.createElement('div');
    document.body.append(host);
    app = mount(Card, {
      target: host,
      props: {
        asked: [{ question: 'Which one?', picked_labels: [], typed_note: null }],
        omitted: 5_200_000,
      },
    });
    flushSync();
    expect(host.querySelector('.card .q use')?.getAttribute('href'), 'the question glyph').toBe(
      '#i-question',
    );
    expect(host.querySelector('.q')?.textContent, 'and the question it asked').toContain(
      'Which one?',
    );
    expect(
      host.querySelector('.patchline .patchmark')?.textContent,
      'the omission on the marks line',
    ).toBe('omitted, 5.0 MB');
    expect(host.querySelector('.a'), 'and no answer row invented').toBeNull();
  });
});
