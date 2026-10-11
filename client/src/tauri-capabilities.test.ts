/**
 * The page's Tauri modules are ACL-gated one by one, and a module with no
 * grant fails at its first call in the shipped app, not in any test: 1.1.7
 * awaited `listen` on the connect path, the app carried no capability, and
 * every desktop and Android app could not connect while every test stayed
 * green. The webview console said it plainly - `event.listen not allowed` -
 * and nothing in the client's own suite reads that console.
 *
 * So this guards the seam the mocks cannot see: every `@tauri-apps/api`
 * module the page imports needs a covering grant under
 * `client/src-tauri/capabilities/`, or an entry in the table below saying it
 * is ungated and why. The scan asserts its own denominator - one that found
 * no imports would guard nothing and must fail rather than pass.
 */

import { existsSync, readFileSync, readdirSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

import { describe, expect, it } from 'vitest';

/** The page's own tree, the scan's whole input. */
const SRC = fileURLToPath(new URL('.', import.meta.url));

/** What the build resolves the app's capabilities from. */
const CAPABILITIES = fileURLToPath(new URL('../src-tauri/capabilities', import.meta.url));

/**
 * What each Tauri module the page may import needs, and `null` for one that
 * is ungated.
 *
 * `core` is ungated: `invoke` reaches the app's own commands (`client_connect`
 * and its siblings), and tauri's ACL check leaves app commands alone on a
 * local origin - only `plugin:` commands are checked (tauri 2.12.0's
 * `webview/mod.rs`, the `plugin_command.is_some()` arm). A module missing
 * from this table is a decision somebody has to make, which is why the
 * lookup asserts rather than defaults.
 */
const GRANTS: Record<string, string | null> = {
  core: null,
  event: 'core:event:',
};

/** `@tauri-apps/api/<module>`, static or dynamic; `''` for the bare root. */
const MODULE = /@tauri-apps\/api(?:\/([a-z0-9-]+))?/g;

/** Every file under `dir`, at any depth. */
function filesUnder(dir: string): string[] {
  const found: string[] = [];
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) found.push(...filesUnder(full));
    else found.push(full);
  }
  return found;
}

/** Every module the page imports, against the files that import it. */
const imported = new Map<string, Set<string>>();
for (const file of filesUnder(SRC)) {
  if (path.basename(file).includes('.test.')) continue;
  const text = readFileSync(file, 'utf8');
  for (const match of text.matchAll(MODULE)) {
    const module = match[1] ?? '';
    const where = imported.get(module) ?? new Set<string>();
    where.add(path.relative(SRC, file));
    imported.set(module, where);
  }
}

/** A permission entry's identifier: a plain string, or a scoped object's. */
function identifierOf(entry: unknown): string {
  if (typeof entry === 'string') return entry;
  if (entry !== null && typeof entry === 'object' && 'identifier' in entry) {
    return typeof entry.identifier === 'string' ? entry.identifier : '';
  }
  return '';
}

/** Every permission identifier the app's capability files grant. */
function granted(): string[] {
  if (!existsSync(CAPABILITIES)) return [];
  const ids: string[] = [];
  for (const name of readdirSync(CAPABILITIES)) {
    if (!name.endsWith('.json')) continue;
    const parsed: unknown = JSON.parse(readFileSync(path.join(CAPABILITIES, name), 'utf8'));
    const permissions =
      parsed !== null && typeof parsed === 'object' && 'permissions' in parsed
        ? parsed.permissions
        : undefined;
    if (!Array.isArray(permissions)) continue;
    for (const entry of permissions) ids.push(identifierOf(entry));
  }
  return ids;
}

describe("the page's Tauri modules", () => {
  /**
   * The denominator. A scan that sees nothing agrees with every grant and
   * guards none of them - the shape of a check that reads healthiest exactly
   * when it is blindest.
   */
  it('finds the imports it guards', () => {
    expect(
      imported.size,
      `the scan found no @tauri-apps/api imports under ${SRC}; it guards nothing`,
    ).toBeGreaterThan(0);
  });

  /**
   * **The one the release needs.** An import of a gated module with no
   * capability grant is a call that works in every mock and denies in the
   * shipped bundle.
   */
  it('has a covering grant for every module the page imports', () => {
    const ids = granted();
    for (const [module, files] of imported) {
      const needed = GRANTS[module];
      expect(
        needed !== undefined,
        `the page imports @tauri-apps/api/${module} (${[...files].join(', ')}) and the table does not say whether it is gated; add it to GRANTS`,
      ).toBe(true);
      if (needed === null || needed === undefined) continue;
      expect(
        ids.some((id) => id.startsWith(needed)),
        `the page imports @tauri-apps/api/${module} (${[...files].join(', ')}), but no capability under client/src-tauri/capabilities grants ${needed}*; the released app denies the call`,
      ).toBe(true);
    }
  });
});
