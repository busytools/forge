import { describe, expect, it } from 'vitest';

import { highlighterFor } from './highlight';

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
});
