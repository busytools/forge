// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { describe, expect, it } from 'vitest';

import Group from './Group.svelte';
import { fold, type Lane } from './units';

/**
 * The lane's rows as MOUNTED DOM, which the server-rendered file cannot pin.
 *
 * A duplicate key is a client-runtime refusal - it aborts the render rather
 * than drawing something wrong - so it only shows up in a mount.
 */
describe('a family of calls, mounted', () => {
  it('draws two calls the wire gave no id, which is one row each and no duplicate key', () => {
    // The fold names an id-less call by the frame and block it arrived in; a
    // view keyed on the wire id alone gives two rows of one family the same
    // empty key, which is Svelte's `each_key_duplicate` and stops the whole
    // turn drawing at mount. So the property this pins is that the rows DRAW.
    const idLess = (uuid: string): unknown => ({
      type: 'assistant',
      uuid,
      message: {
        id: 'm1',
        role: 'assistant',
        model: 'claude-opus-5',
        content: [{ type: 'tool_use', name: 'Bash', input: { command: 'just check' } }],
      },
    });
    const units = fold([idLess('a1'), idLess('a2')]);
    const group = units[0];
    if (group?.kind !== 'group') throw new Error('the fold drew no group to draw');

    const app = mount(Group, { target: document.body, props: { lanes: group.lanes } });
    try {
      flushSync();
      expect(document.querySelectorAll('.lane .leaf').length, 'both calls drew').toBe(2);
    } finally {
      void unmount(app);
      document.body.innerHTML = '';
    }
  });

  it('draws a list whose seat rows share the word lead, which is one row each and no duplicate key', () => {
    // **Every project's own agent is labelled `lead`**, so the ordinary
    // unfiltered `agents__list` - the documented health check - carries two
    // rows with that word. Keyed by the label they are one key, and Svelte
    // refuses a duplicate key at mount: the whole turn stops drawing.
    const lanes: Lane[] = [
      {
        tag: 'message',
        cards: [
          {
            id: 'm-1',
            row: 'list',
            peer: 'list',
            body: '',
            org: null,
            status: 'completed',
            ack: null,
            seat: null,
            seats: [
              {
                org: 'Busytools',
                label: 'lead',
                project: 'forge',
                what: 'this session',
                liveness: '',
              },
              {
                org: 'Gateway',
                label: 'lead',
                project: 'gateway-backend',
                what: 'another project',
                liveness: '',
              },
            ],
          },
        ],
      },
    ];

    const app = mount(Group, { target: document.body, props: { lanes } });
    try {
      flushSync();
      expect(document.querySelectorAll('details.leaf .kv').length, 'both seat rows drew').toBe(2);
      expect(document.body.textContent, 'under their own projects').toContain('gateway-backend');
    } finally {
      void unmount(app);
      document.body.innerHTML = '';
    }
  });
});
