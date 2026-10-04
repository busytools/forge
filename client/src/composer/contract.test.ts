import { describe, expect, it } from 'vitest';

import { homeWire } from '../dev/fixture.data';
import session from '../dev/fixtures/session.json';
import { seatState } from '../session/view';
import { sessionFrom } from '../session/wire';
import { dictationOffered, type ComposerConnection, type ComposerProps } from './view';

/** Nothing is dispatched here: the check is the assignment, not a render. */
const idle = {
  dispatch: () => null,
  devices: () => true,
  onMessage: () => () => {},
  store: () => undefined,
} as unknown as ComposerConnection;

/**
 * The seam the session page hands the composer across.
 *
 * **The assertion is the assignment.** A prop the page does not carry, or a
 * field it carries in another shape, is a compile error here rather than a
 * composer that draws an empty list on a live socket - which is the failure a
 * structural type exists to make loud. `pendingDepth` was exactly that: the one
 * read the page owed the dock's queue line, found by this shape rather than by
 * looking at a screenshot.
 */
describe("the composer's props as the session page builds them", () => {
  it('take the record and the seat the page holds, with no cast', () => {
    const slot = { org: 'TestOrg', project: 'proj', label: 'lead' };
    const props: ComposerProps = {
      record: sessionFrom(session),
      slot,
      seat: seatState(homeWire, slot),
      connection: idle,
      // Read the way the router reads it, so the seam covers the whole prop
      // set rather than the four the record carries.
      dictation: dictationOffered(homeWire.dictate),
    };

    expect(
      props.seat.pendingDepth,
      'the seat carries the depth the dock states behind its prompt',
    ).toBeTypeOf('number');
    expect(props.record.pending_ask, 'and the record carries whatever is waiting').toBeDefined();
  });
});
