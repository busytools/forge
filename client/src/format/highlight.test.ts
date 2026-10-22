import { describe, expect, it } from 'vitest';

import { highlightedBody, highlighterFor } from './highlight';

describe('the shared highlighter', () => {
  /**
   * **The reader's bridge asks `getLanguage('text')`**, and shiki treats
   * `text` as special only inside its own tokenisers - so without a
   * registration the plain-text path throws live (measured on the dev stack,
   * `ShikiError: Language 'text' not found`).
   */
  it('registers a grammar under the special text name the reader asks for', async () => {
    const highlighter = await highlighterFor('text');
    expect(highlighter.getLoadedLanguages()).toContain('text');
  });

  /**
   * **The chat's Read panel and its fences are the same theme hex for hex.**
   * Shiki's `github-dark` is the older GitHub palette and the highlight.js
   * stylesheet the fences use is the newer one, so tokenising with shiki's
   * own theme recolours a Read row away from the fence beside it. Pinned on
   * the classes a reader compares first; the old shiki hexes must not appear.
   */
  it('paints a Read in the fenced blocks own highlight.js palette', async () => {
    const body = (
      await highlightedBody(
        'rust',
        'use std::io;\n/// the owner is the session\'s\nfn open() -> String { "forge".to_owned() }\n',
      )
    ).toLowerCase();

    expect(body, 'keyword, the highlight.js palette').toContain('#ff7b72');
    expect(body, 'string').toContain('#a5d6ff');
    expect(body, 'comment').toContain('#8b949e');
    expect(body, 'entity').toContain('#d2a8ff');
    expect(body, 'text').toContain('#c9d1d9');
    expect(body, "shiki's older palette must not leak in").not.toMatch(
      /#f97583|#9ecbff|#6a737d|#b392f0|#e1e4e8/,
    );
  });
});
