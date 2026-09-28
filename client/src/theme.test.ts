import { describe, expect, it } from 'vitest';

import { fontStack, rootTokens } from './theme';
import { FONT_NAMES, THEME_NAMES } from './wire/types';

describe('the tokens the server sends', () => {
  it('resolves the palette for an unset name and for the shipped one alike', () => {
    expect(rootTokens(null)).toEqual(rootTokens(THEME_NAMES[0]));
  });

  it('resolves every token the stylesheet reads', () => {
    const tokens = rootTokens(null);
    for (const token of ['--bg', '--text', '--accent', '--line', '--ok', '--warn', '--bad']) {
      expect(tokens).toHaveProperty(token);
    }
  });

  it('draws the shipped font name as its own stack, not the built-in pair', () => {
    const name = FONT_NAMES[0];
    expect(fontStack(name)).not.toBeNull();
    expect(fontStack(name)).not.toEqual(fontStack(null));
  });

  it('draws no stack for a name outside the shipped set', () => {
    const outside = 'comic';
    expect(FONT_NAMES).not.toContain(outside);
    expect(fontStack(outside)).toBeNull();
  });
});
