/**
 * The fixture as a typed constant, for TESTS.
 *
 * **Nothing the shipped app imports may import this file.** It reads the
 * JSON with a static import, which defeats tree-shaking and puts the fixture
 * in the bundle - which is exactly what `fixture.test.ts` builds the app and
 * checks for. The dev route reaches the fixture through `loadFixtureHome`,
 * which defers the import behind the DEV guard.
 */

import { boardFrom, type BoardWire } from '../wire/board';
import { homeFrom, type HomeWire } from '../wire/home';
import board from './fixtures/board.json';
import home from './fixtures/home.json';

export const homeWire: HomeWire = homeFrom(home as unknown as HomeWire);
// The fixture is the shape the server writes, so the narrowing answers one;
// a null here would be the fixture going stale, which every page test using
// it would fail on anyway.
export const boardWire: BoardWire =
  boardFrom(board as unknown as BoardWire) ?? (board as unknown as BoardWire);
