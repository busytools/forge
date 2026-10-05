import { readFileSync } from 'node:fs';

import { describe, expect, it } from 'vitest';

import { CLIENT_VERSION, MIN_PROTOCOL, PROTOCOL_VERSION, skewMessage } from './protocol';

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

/**
 * The release the client's own manifest declares, read out of it.
 *
 * The bake is a Vite define over this file, so a define pointed at some
 * other number would still produce a non-empty version and a message that
 * names the wrong build at the one moment the message matters.
 */
function clientRelease(): string {
  const source = readFileSync(new URL('../src-tauri/Cargo.toml', import.meta.url), 'utf8');
  const declared = /^version = "([^"]+)";?$/m.exec(source);
  if (declared === null || declared[1] === undefined) {
    throw new Error(
      'client/src-tauri/Cargo.toml no longer declares a `version` in the shape this pin reads, ' +
        'so the client is no longer naming its own release',
    );
  }
  return declared[1];
}

describe('the release the client names itself by', () => {
  it('is the number `just release` sets on the client half', () => {
    expect(CLIENT_VERSION).toBe(clientRelease());
  });
});

describe('the floor this client tolerates', () => {
  /**
   * The floor is an act, not a derivation: one step back is kept only while
   * that step's read coverage is pinned by the floor fixture, so a bump
   * moves this deliberately or not at all. `PROTOCOL_VERSION - 1` would
   * move it silently with every bump.
   */
  it('is pinned one step back, not derived from the version', () => {
    expect(MIN_PROTOCOL).toBe(4);
    expect(MIN_PROTOCOL).toBeLessThan(PROTOCOL_VERSION);
  });
});

describe('what a skew says', () => {
  /**
   * The reader has to be able to tell which half is stale and what to run:
   * two protocol numbers alone name neither build and no way out.
   */
  it('names both halves and the command when the server named its build', () => {
    expect(skewMessage({ serverProtocol: 4, serverVersion: '1.0.112+abc1234' })).toBe(
      `this forge server is v1.0.112+abc1234 (protocol 4); this client is v${CLIENT_VERSION} ` +
        `(protocol ${PROTOCOL_VERSION}). In the forge checkout run \`just install\` and ` +
        'restart forge.',
    );
  });

  /**
   * Every server this client will refuse for a protocol reason predates the
   * greeting's release fields, so the common case has no build to name -
   * and a message that dropped the half it does know would leave the reader
   * with nothing.
   */
  it('names the half it knows when the server is too old to name its build', () => {
    expect(skewMessage({ serverProtocol: 3, serverVersion: null })).toBe(
      `this forge server speaks protocol 3; this client is v${CLIENT_VERSION} ` +
        `(protocol ${PROTOCOL_VERSION}). In the forge checkout run \`just install\` and ` +
        'restart forge.',
    );
  });

  /**
   * A server ahead of this client is the other direction, and the half to
   * fix is this one: sending the reader to rebuild the server would be
   * advice that changes nothing.
   */
  it('names this client as the half behind when the server is ahead', () => {
    expect(
      skewMessage({ serverProtocol: PROTOCOL_VERSION + 1, serverVersion: '1.0.116+abc1234' }),
    ).toBe(
      `this forge server is v1.0.116+abc1234 (protocol ${PROTOCOL_VERSION + 1}); this client is ` +
        `v${CLIENT_VERSION} (protocol ${PROTOCOL_VERSION}). This client is the half that is ` +
        'behind, so reinstall it from the checkout that built the server.',
    );
  });

  /**
   * One release with two protocols is one build installed twice, and the
   * fix is both halves rather than the server alone.
   */
  it('calls one release speaking two protocols a mixed install', () => {
    expect(skewMessage({ serverProtocol: 4, serverVersion: `${CLIENT_VERSION}+abc1234` })).toBe(
      `this forge server and this client are both v${CLIENT_VERSION}, but they speak protocols ` +
        `4 and ${PROTOCOL_VERSION}: a mixed install of one build. Reinstall both. In the forge ` +
        'checkout run `just install` and restart forge.',
    );
  });
});
