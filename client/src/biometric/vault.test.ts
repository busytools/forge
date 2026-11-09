/**
 * The vault purpose's blob, which is the consumer's own shape: the gate never
 * looks inside it, so what a later start can read back is this module's
 * contract.
 */
import { describe, expect, it } from 'vitest';

import { decodeVaultMaterial, encodeVaultMaterial } from './vault';

describe('the vault material', () => {
  it('round-trips the refresh token and the user key', () => {
    const material = { refresh_token: 'refresh-1', user_key: 'base64-user-key' };
    expect(decodeVaultMaterial(encodeVaultMaterial(material))).toEqual(material);
  });

  it('refuses a blob that is not this shape', () => {
    expect(decodeVaultMaterial('not json')).toBeNull();
    expect(decodeVaultMaterial('null')).toBeNull();
    expect(decodeVaultMaterial('[]')).toBeNull();
    expect(decodeVaultMaterial('{"refresh_token":"r"}')).toBeNull();
    expect(decodeVaultMaterial('{"user_key":"u"}')).toBeNull();
    expect(decodeVaultMaterial('{"refresh_token":"","user_key":"u"}')).toBeNull();
    expect(decodeVaultMaterial('{"refresh_token":1,"user_key":"u"}')).toBeNull();
  });
});
