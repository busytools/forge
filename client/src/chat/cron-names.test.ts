import { describe, expect, it } from 'vitest';

import { cronNames } from './cron-names.svelte';

/**
 * The pairing is one short string per fired prompt's uuid, and nothing pruned
 * it: a client left open through a long run of cron fires kept every one.
 */
describe('the schedule names a fired prompt leaves behind', () => {
  it('keeps the newest pairings and drops the oldest past the cap', () => {
    for (let at = 0; at < 600; at += 1) cronNames.remember(`p-${at}`, `schedule ${at}`);

    expect(cronNames.nameFor('p-599'), 'the newest pairing was pruned').toBe('schedule 599');
    expect(cronNames.nameFor('p-0'), 'a pairing past the cap stayed').toBeNull();
  });
});
