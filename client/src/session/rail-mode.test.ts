// @vitest-environment jsdom
import { afterEach, describe, expect, it } from 'vitest';

import { rememberedRailMode, rememberRailMode } from './rail-mode';

afterEach(() => {
  localStorage.removeItem('forge.rail');
});

describe('the rail preference', () => {
  /** The default is the column - what the desk wants to wake up to. */
  it('defaults to the static column', () => {
    expect(rememberedRailMode(), 'the default was not the column').toBe('static');
  });

  it('round-trips a chosen mode and forgets junk', () => {
    rememberRailMode('hover');
    expect(rememberedRailMode()).toBe('hover');
    rememberRailMode('closed');
    expect(rememberedRailMode()).toBe('closed');

    localStorage.setItem('forge.rail', 'sideways');
    expect(rememberedRailMode(), 'junk read as a mode').toBe('static');
  });
});
