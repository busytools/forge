// @vitest-environment jsdom
/**
 * The chat's code panel: the grammar is handed to it by the row, it colours
 * with the fenced blocks' highlight.js palette, and a file with no grammar
 * draws plain with the header note.
 */
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

import Code from './Code.svelte';

/** Mount the panel with `language` and wait for the async highlighter. */
async function panel(language: string | null, text: string): Promise<HTMLElement> {
  const app = mount(Code, { target: document.body, props: { language, text } });
  try {
    await vi.waitFor(() => {
      flushSync();
      // The plain fallback is synchronous; the grammar's own spans are not.
      if (document.querySelector('.code pre span[style*="color"]') === null) {
        throw new Error('waiting for the highlighter');
      }
    });
    const drawn = document.querySelector<HTMLElement>('.code');
    if (drawn === null) throw new Error('the panel drew nothing');
    return drawn;
  } finally {
    void unmount(app);
  }
}

afterEach(() => {
  document.body.innerHTML = '';
});

describe('a read the policy calls code', () => {
  it('colours a Justfile with the just grammar and names it in the header', async () => {
    const drawn = await panel('just', 'set shell := ["bash", "-cu"]\n\ncheck:\n  cargo fmt');

    expect(drawn.querySelector('.lang')?.textContent, 'the grammar id names the panel').toBe(
      'just',
    );
    expect(
      [...drawn.querySelectorAll('pre span')].some((span) => span.textContent === 'set'),
      'shiki tokenised the file, not plain spans',
    ).toBe(true);
    expect(drawn.innerHTML, 'the tokens carry the theme colours').toMatch(/color:\s*#/);
  });

  it('colours a Makefile with the make grammar', async () => {
    const drawn = await panel('make', 'check:\n\tcargo fmt --check');

    expect(drawn.querySelector('.lang')?.textContent, 'the grammar names the panel').toBe('make');
    expect(drawn.innerHTML).toMatch(/color:\s*#/);
  });
});

describe('a read the policy calls text', () => {
  it('draws plain, with no language in the header and the note in the caption', () => {
    const app = mount(Code, {
      target: document.body,
      props: { language: null, text: 'nothing here matches a grammar' },
    });
    try {
      flushSync();
      const drawn = document.querySelector<HTMLElement>('.code');
      expect(drawn, 'the panel still draws').not.toBeNull();
      expect(drawn?.querySelector('.lang'), 'no language name for a file with none').toBeNull();
      expect(document.body.textContent).toContain('plain text - no syntax for this file');
      expect(drawn?.textContent, 'and every word of the file stays').toContain(
        'nothing here matches a grammar',
      );
    } finally {
      void unmount(app);
    }
  });
});
