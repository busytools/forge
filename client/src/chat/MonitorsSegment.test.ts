import { render } from 'svelte/server';
import { afterEach, describe, expect, it } from 'vitest';

import MonitorsSegment from './MonitorsSegment.svelte';
import { monitors } from './monitors.svelte';

afterEach(() => {
  monitors.sync(null);
});

describe('the monitors segment', () => {
  it('carries the monitors glyph and the running count', () => {
    monitors.sync([
      {
        id: 'm1',
        running: true,
        completed: false,
        name: 'ci-watch',
        label: 'persistent',
        command: 'gh run watch',
      },
      {
        id: 'm2',
        running: false,
        completed: false,
        name: 'log-tail',
        label: 'stopped',
        command: 'tail -f f.log',
      },
    ]);
    const body = render(MonitorsSegment, {}).body;

    expect(body, 'the glyph says what the segment is').toContain('i-monitors');
    expect(body, 'the running count reads on the toggle').toContain('1 running');
  });

  it('draws nothing for a session watching nothing', () => {
    monitors.sync(null);
    const body = render(MonitorsSegment, {}).body;

    expect(body, 'no segment at all').not.toContain('sg-seg');
  });
});
