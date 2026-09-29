import { describe, expect, it } from 'vitest';

import { clock, duration, grouped, money, tokens } from './numbers';

describe('the figures a turn row draws', () => {
  it('draws a span in the unit it reads best in', () => {
    expect(duration(400)).toBe('0.4s');
    expect(duration(15_000)).toBe('15.0s');
    expect(duration(79_000)).toBe('1m 19s');
    expect(duration(161_000)).toBe('2m 41s');
    expect(duration(null)).toBe('-');
  });

  it('draws a token count short on a row and grouped in a body', () => {
    expect(tokens(18_400)).toBe('18.4k');
    expect(tokens(4_231)).toBe('4.2k');
    expect(tokens(942)).toBe('942');
    expect(grouped(108_442)).toBe('108,442');
    expect(grouped(4_231)).toBe('4,231');
  });

  it('draws money as the CLI reported it', () => {
    expect(money(4.82)).toBe('$4.82');
    expect(money(0.004)).toBe('$0.00');
  });

  it('draws the instant a turn ended in the zone of the reader', () => {
    // The record carries the instant and never a formatted clock: which zone
    // a reader is in is the view's to know, and a string baked at write time
    // outlives the zone it was baked in.
    const at = new Date(2026, 8, 29, 15, 48, 31).toISOString();
    expect(clock(at)).toBe('15:48:31');
    expect(clock(null)).toBeNull();
    expect(clock('not an instant'), 'an instant that does not parse draws nothing').toBeNull();
  });
});
