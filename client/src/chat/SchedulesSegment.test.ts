import { render } from 'svelte/server';
import { afterEach, describe, expect, it } from 'vitest';

import SchedulesSegment from './SchedulesSegment.svelte';
import { schedules } from './schedules.svelte';

afterEach(() => {
  schedules.sync(null);
});

describe('the schedules segment', () => {
  it('carries the schedule glyph, the count and the wording', () => {
    schedules.sync([
      { id: 'c-1', key: 'rules sweep', value: 'in 27d \u{b7} recurring' },
      { id: 'c-2', key: 'plugin audit', value: 'in 27d \u{b7} one-shot' },
    ]);
    const body = render(SchedulesSegment, {}).body;

    expect(body, 'the glyph says what the segment is').toContain('i-schedules');
    expect(body, 'the plural count reads plainly').toContain('2 schedules');
  });

  it('counts a single schedule in the singular', () => {
    schedules.sync([{ id: 'c-1', key: 'rules sweep', value: 'in 27d \u{b7} recurring' }]);
    const body = render(SchedulesSegment, {}).body;

    expect(body, 'one reads as one').toContain('1 schedule');
    expect(body, 'and not as a plural').not.toContain('1 schedules');
  });

  it('draws nothing for a project with no schedules', () => {
    schedules.sync(null);
    const body = render(SchedulesSegment, {}).body;

    expect(body, 'no segment at all').not.toContain('sg-seg');
  });
});
