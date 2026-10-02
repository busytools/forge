import { readFileSync } from 'node:fs';

import { describe, expect, it } from 'vitest';

import { PROTOCOL_VERSION } from './protocol';

/**
 * The protocol version the server declares, read out of its source.
 *
 * The two are hand-kept and neither is generated from the other, so a drift
 * between them is a client that refuses every connection over a mismatch
 * naming neither file. Nothing else catches it: the greeting the client
 * compares against is built from this same constant, so the frames on the
 * wire carry whichever value `transport.rs` holds and agree with it by
 * construction. The one place the server's copy can be read from is its
 * source, the way `wire/fleet.test.ts` reads `live.rs`.
 *
 * **The read assumes a shape, and changing it means re-checking this rather
 * than trusting a green:** the constant is still `PROTOCOL_VERSION`, still a
 * `u32`, and still one line written `pub const PROTOCOL_VERSION: u32 =
 * <digits>;`. A declaration this cannot find throws rather than answering,
 * because a pin that has stopped reading anything looks exactly like one
 * that agrees.
 */
function serverProtocolVersion(): number {
  const source = readFileSync(
    new URL('../../crates/forge-server/src/transport.rs', import.meta.url),
    'utf8',
  );
  const declared = /^pub const PROTOCOL_VERSION: u32 = (\d+);$/m.exec(source);
  if (declared === null || declared[1] === undefined) {
    throw new Error(
      'transport.rs no longer declares `PROTOCOL_VERSION` in the shape this pin reads, so the ' +
        'two versions are no longer being compared',
    );
  }
  return Number(declared[1]);
}

describe('the protocol version', () => {
  /**
   * A bump on either side alone is a client that cannot connect at all, so
   * the two numbers move together or this goes red.
   */
  it("agrees with the server's", () => {
    expect(PROTOCOL_VERSION).toBe(serverProtocolVersion());
  });
});
