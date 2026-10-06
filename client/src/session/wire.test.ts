import { describe, expect, it } from 'vitest';

import { backgroundTaskFrom } from './wire';

describe('the background-task registry parse', () => {
  it('drops a row missing its id, its kind or its words rather than drawing it blank', () => {
    // The plausible regression is a forgiving parse (`?? ''`), which draws a
    // row a reader cannot act on and whose absence nothing else would report.
    expect(backgroundTaskFrom({ task_type: 'local_bash', description: 'x' }), 'no id').toEqual([]);
    expect(backgroundTaskFrom({ task_id: 't', description: 'x' }), 'no kind').toEqual([]);
    expect(backgroundTaskFrom({ task_id: 't', task_type: 'local_bash' }), 'no words').toEqual([]);
    expect(
      backgroundTaskFrom({ task_id: 't', task_type: 'local_bash', description: 'x' }),
      'a full row parses, with no command',
    ).toEqual([
      { task_id: 't', task_type: 'local_bash', description: 'x', command: null, tool_use_id: null },
    ]);
  });
});
