import { describe, expect, it } from 'vitest';

import { isNewer, versionParts } from './version';

describe('versionParts', () => {
  it('reads a release as three integers', () => {
    expect(versionParts('1.0.116')).toEqual([1, 0, 116]);
    expect(versionParts('v1.0.116')).toEqual([1, 0, 116]);
    expect(versionParts(' v1.0.116 ')).toEqual([1, 0, 116]);
  });

  /**
   * A compare that dropped the segments it could not read would take
   * `x.1.0.116` for `1.0.116` and offer it.
   */
  it('refuses anything that is not three integers', () => {
    for (const odd of ['1.0.116-rc1', '1.0.116.1', '1.0.116-beta', 'x.1.0.116', '1.0', '', 'v']) {
      expect(versionParts(odd), odd).toBeNull();
    }
  });

  /**
   * **A build stamp after `+` is dropped, semver's own rule.** The client's
   * own version carries one now, and a compare that refused it would stop
   * offering updates to every stamped build.
   */
  it('drops the build stamp after a plus', () => {
    expect(versionParts('1.0.116+65c8a7695')).toEqual([1, 0, 116]);
    expect(versionParts('v1.0.116+abc')).toEqual([1, 0, 116]);
    expect(isNewer('1.1.0+def', '1.0.116+abc')).toBe(true);
    expect(isNewer('1.0.116+abc', '1.0.116+def')).toBe(false);
  });
});

describe('isNewer', () => {
  it('is strictly newer or not at all', () => {
    expect(isNewer('1.0.116', '1.0.115')).toBe(true);
    expect(isNewer('1.1.0', '1.0.115')).toBe(true);
    expect(isNewer('2.0.0', '1.0.115')).toBe(true);
    expect(isNewer('1.0.115', '1.0.115')).toBe(false);
    expect(isNewer('1.0.114', '1.0.115')).toBe(false);
  });

  it('reads a v prefix on either side', () => {
    expect(isNewer('v1.0.116', '1.0.115')).toBe(true);
    expect(isNewer('1.0.116', 'v1.0.115')).toBe(true);
  });

  it('answers no when either side will not parse', () => {
    expect(isNewer('1.0.116-rc1', '1.0.115')).toBe(false);
    expect(isNewer('1.0.116', '1.0.115-rc1')).toBe(false);
  });
});
