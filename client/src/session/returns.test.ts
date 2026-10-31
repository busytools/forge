import { describe, expect, it } from 'vitest';

import type { Connection } from '../socket';
import type { SessionSlot } from '../wire/types';
import { markReturn, takeReturn } from './returns';

const LEAD: SessionSlot = { org: 'Busytools', project: 'forge', label: 'lead' };
const OTHER: SessionSlot = { org: 'Busytools', project: 'forge', label: 'other' };

/** The mark is keyed by connection and seat, and nothing here talks to one. */
const connection = {} as unknown as Connection;

describe('the return mark', () => {
  it("is the seat's own: another seat takes nothing from it", () => {
    markReturn(connection, LEAD);
    expect(takeReturn(connection, OTHER), 'another seat does not spend it').toBe(false);
    expect(takeReturn(connection, LEAD), 'the seat it names does').toBe(true);
    expect(takeReturn(connection, LEAD), 'and only once').toBe(false);
  });
});
