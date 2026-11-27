import { describe, expect, it } from 'vitest';

import { LIVENESS_DEAD_MS, LIVENESS_EVERY_MS } from './socket';
import { SILENCE_MS, silenceOf } from './silence';

/** An open link whose last frame landed at 1 s, on a server that probes. */
const OPEN = { status: 'open' as const, protocol: 20, heardAt: 1_000 };

describe('a link that is open but has gone quiet', () => {
  /**
   * **Below the transports' own window is a reconnect, not a silence.** A
   * browser page's probe goes out after 15 s quiet and reaps at 65 s; the
   * shell's byte-counted beat reaps sooner. Both report a status the strip
   * already draws, so this line must not pre-empt them.
   */
  it('says nothing inside the transport own detect window', () => {
    expect(silenceOf({ ...OPEN, now: 1_000 + SILENCE_MS - 1 })).toBeNull();
  });

  it('reports the quiet once nothing has been heard for the bound', () => {
    expect(silenceOf({ ...OPEN, now: 1_000 + SILENCE_MS })).toBe(SILENCE_MS);
    expect(silenceOf({ ...OPEN, now: 1_000 + SILENCE_MS * 90 })).toBe(SILENCE_MS * 90);
  });

  it('sits past the browser transport own detect bound', () => {
    expect(SILENCE_MS).toBeGreaterThan(LIVENESS_EVERY_MS + LIVENESS_DEAD_MS);
  });

  it('leaves a link the status already calls gone to the reconnect line', () => {
    expect(silenceOf({ ...OPEN, status: 'connecting', now: 1_000 + SILENCE_MS })).toBeNull();
    expect(silenceOf({ ...OPEN, status: 'closed', now: 1_000 + SILENCE_MS })).toBeNull();
  });

  /**
   * **A server older than the probe cannot answer one**, so its quiet is not
   * evidence of anything and must never be drawn as a fault.
   */
  it('never calls a server that cannot answer the probe silent', () => {
    expect(silenceOf({ ...OPEN, protocol: 19, now: 1_000 + SILENCE_MS * 10 })).toBeNull();
  });

  /** A link that never greeted is dead however old the server is. */
  it('reports a link that has not even greeted', () => {
    expect(silenceOf({ ...OPEN, protocol: null, now: 1_000 + SILENCE_MS })).toBe(SILENCE_MS);
  });

  it('has no silence to measure before an open stamped one', () => {
    expect(silenceOf({ ...OPEN, heardAt: null, now: 1_000 + SILENCE_MS })).toBeNull();
  });
});
