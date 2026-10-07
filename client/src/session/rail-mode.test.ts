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

  it('round-trips a chosen mode and forgets the rest', () => {
    rememberRailMode('hover');
    expect(rememberedRailMode()).toBe('hover');
    rememberRailMode('static');
    expect(rememberedRailMode()).toBe('static');

    // Junk, and a `closed` from before the close went, both read as the
    // default rather than pinning a rail nothing can put away.
    localStorage.setItem('forge.rail', 'closed');
    expect(rememberedRailMode(), 'a stale closed read as a mode').toBe('static');
    localStorage.setItem('forge.rail', 'sideways');
    expect(rememberedRailMode(), 'junk read as a mode').toBe('static');
  });
});
