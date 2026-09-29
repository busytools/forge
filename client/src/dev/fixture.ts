/**
 * The fixture as the DEVELOPMENT route reads it, and nothing the shipped app
 * reads at all.
 *
 * **The dynamic import behind the DEV guard is what keeps it out of the
 * production bundle.** A static import defeats tree-shaking, so a dev-only
 * route guarded any other way would still ship the fixture - and the app's
 * only input is the server URL, so bundled data it could fall back to is the
 * failure `fixture.test.ts` exists to catch.
 *
 * The typed constant the tests use is in `fixture.data.ts`, which IS a
 * static import and must not be reachable from anything the app builds.
 */

import { homeFrom, type HomeWire } from '../wire/home';

/** The fixture home, or `null` in a production build. */
export async function loadFixtureHome(): Promise<HomeWire | null> {
  if (!import.meta.env.DEV) return null;
  const module = await import('./fixtures/home.json');
  // The fixture's literals widen to `string` through `resolveJsonModule`, and
  // `homeFrom` narrows every union member straight afterwards.
  return homeFrom(module.default as unknown as HomeWire);
}
