/**
 * The fixture as a typed constant, for TESTS.
 *
 * **Nothing the shipped app imports may import this file.** It reads the
 * JSON with a static import, which defeats tree-shaking and puts the fixture
 * in the bundle - which is exactly what `fixture.test.ts` builds the app and
 * checks for. The dev route reaches the fixture through `loadFixtureHome`,
 * which defers the import behind the DEV guard.
 */

import { homeFrom, type HomeWire } from '../wire/home';
import home from './fixtures/home.json';

export const homeWire: HomeWire = homeFrom(home as unknown as HomeWire);
