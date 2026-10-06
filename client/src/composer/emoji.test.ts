import { describe, expect, it } from 'vitest';

import { shortcodeQuery } from './emoji';

/**
 * The scan that decides which `:shortcode` the draft ends in.
 *
 * **It reads backwards from the end and stops at the first character that
 * cannot be in a shortcode**, which is what keeps a keystroke on a pasted
 * draft cheap (#1647); these pin the behaviour that scan must keep, including
 * the surrogate case the code-unit walk has to answer the same way.
 */
describe('the shortcode scan', () => {
  it('reads the shortcode the text ends in', () => {
    expect(shortcodeQuery(':sm'), 'the ordinary tail lost its query').toBe('sm');
    expect(shortcodeQuery('the :s'), 'a colon after whitespace did not open').toBe('s');
    expect(shortcodeQuery(':s'), 'a colon at the start did not open').toBe('s');
  });

  it('opens nothing for a colon that is not a shortcode opener', () => {
    expect(shortcodeQuery('http://x'), 'a URL opened a query').toBeNull();
    expect(shortcodeQuery('10:30'), 'a time opened a query').toBeNull();
    expect(shortcodeQuery('note:'), 'a bare trailing colon opened a query').toBeNull();
    expect(shortcodeQuery('hello'), 'text with no colon claimed a query').toBeNull();
  });

  it('closes the token on anything that cannot be in a shortcode', () => {
    expect(shortcodeQuery(':sm '), 'a space did not close the token').toBeNull();
    expect(
      shortcodeQuery(':sm\u{1F642}'),
      'an emoji after the colon did not close the token',
    ).toBeNull();
  });
});
