/**
 * The page's Tauri modules are ACL-gated one by one, and a module with no
 * grant fails at its first call in the shipped app, not in any test: 1.1.7
 * awaited `listen` on the connect path, the app carried no capability, and
 * every desktop and Android app could not connect while every test stayed
 * green. The webview console said it plainly - `event.listen not allowed` -
 * and nothing in the client's own suite reads that console.
 *
 * So this guards the seam the mocks cannot see: every `@tauri-apps/`
 * package the page imports - the api submodules and the plugin packages
 * alike - needs a covering grant under `client/src-tauri/capabilities/`, or
 * an entry in the table below saying it is ungated and why. The scan asserts
 * its own denominator - one that found no imports, or that missed a seam the
 * guard exists for, must fail rather than pass.
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
 * is ungated. Keys are the api submodule (`core`, `event`) or a plugin
 * package's own name, which is the grain its permissions live at.
 *
 * `core` is ungated: `invoke` reaches the app's own commands (`client_connect`
 * and its siblings), and tauri's gate is `plugin_command.is_some() ||
 * has_app_acl_manifest || !is_local` (tauri 2.12.0's `webview/mod.rs:2085`).
 * This app defines no ACL manifest and serves the page from a local origin,
 * so only the `plugin:` arm bites here. A module missing from this table is a
 * decision somebody has to make, which is why the lookup asserts rather than
 * defaults.
 */
const GRANTS: Record<string, string | null> = {
  core: null,
  event: 'core:event:',
};

/**
 * The modules this guard exists for: every table entry that carries a grant.
 * A scan that found imports but missed one of these is blind to the seam it
 * was written for, however healthy its count looks - so the denominator
 * below names them rather than trusting a non-empty map.
 */
const GUARDED = Object.entries(GRANTS)
  .filter(([, needed]) => needed !== null)
  .map(([module]) => module);

/**
 * Every `@tauri-apps/` specifier, static or dynamic: the api package with or
 * without a submodule (`@tauri-apps/api`, `@tauri-apps/api/core`), and the
 * plugin packages (`@tauri-apps/plugin-*`), whose two-segment form is the
 * package itself.
 */
const TAURI_PACKAGE = /@tauri-apps\/([a-z0-9-]+)(?:\/([a-z0-9-]+))?/g;

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

/**
 * Every module the page imports, against the files that import it: the api
 * submodule for `@tauri-apps/api`, and the package's own name for anything
 * else - so a plugin package the table does not decide fails the guard
 * rather than slipping past a scan that only knew one package.
 */
const imported = new Map<string, Set<string>>();
for (const file of filesUnder(SRC)) {
  if (path.basename(file).includes('.test.')) continue;
  const text = readFileSync(file, 'utf8');
  for (const match of text.matchAll(TAURI_PACKAGE)) {
    const pkg = match[1] ?? '';
    const module = pkg === 'api' ? (match[2] ?? '') : pkg;
    const where = imported.get(module) ?? new Set<string>();
    where.add(path.relative(SRC, file));
    imported.set(module, where);
  }
}

/** The import specifier a scan key stands for, for messages that name it. */
function specifierOf(module: string): string {
  if (module === '') return '@tauri-apps/api';
  return module.startsWith('plugin-') ? `@tauri-apps/${module}` : `@tauri-apps/api/${module}`;
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
   * when it is blindest. Non-emptiness alone is not enough of a denominator:
   * a scan that found only `core` passes it while the gated seam goes
   * unguarded, so every module the guard exists for must be in the map.
   */
  it('finds the imports it guards', () => {
    expect(
      imported.size,
      `the scan found no @tauri-apps/api imports under ${SRC}; it guards nothing`,
    ).toBeGreaterThan(0);
    for (const module of GUARDED) {
      expect(
        imported.has(module),
        `the scan did not find the page's ${specifierOf(module)} import, so the guard is blind to the seam it exists for`,
      ).toBe(true);
    }
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
        `the page imports ${specifierOf(module)} (${[...files].join(', ')}) and the table does not say whether it is gated; add it to GRANTS`,
      ).toBe(true);
      if (needed === null || needed === undefined) continue;
      expect(
        ids.some((id) => id.startsWith(needed)),
        `the page imports ${specifierOf(module)} (${[...files].join(', ')}), but no capability under client/src-tauri/capabilities grants ${needed}*; the released app denies the call`,
      ).toBe(true);
    }
  });
});
