// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { describe, expect, it } from 'vitest';

import Leaves from './Leaves.svelte';
import { fold, type WorkRow } from './units';

/**
 * The list's rows as MOUNTED DOM, which the server-rendered file cannot pin.
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
    if (group?.kind !== 'leaves') throw new Error('the fold drew no rows to draw');

    const app = mount(Leaves, { target: document.body, props: { rows: group.rows } });
    try {
      flushSync();
      expect(document.querySelectorAll('.leaves .leaf').length, 'both calls drew').toBe(2);
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
    const rows: WorkRow[] = [
      {
        tag: 'card',
        card: {
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
      },
    ];

    const app = mount(Leaves, { target: document.body, props: { rows } });
    try {
      flushSync();
      // The seats' facts ride the body, which a closed row no longer holds:
      // the row is opened the way a reader opens it, then counted.
      const card = document.querySelector('details.leaf');
      if (!(card instanceof HTMLDetailsElement)) throw new Error('the card row drew no details');
      card.open = true;
      card.dispatchEvent(new Event('toggle'));
      flushSync();
      expect(document.querySelectorAll('details.leaf .kv').length, 'both seat rows drew').toBe(2);
      expect(document.body.textContent, 'under their own projects').toContain('gateway-backend');
    } finally {
      void unmount(app);
      document.body.innerHTML = '';
    }
  });

  it('holds nothing under a closed card and draws its body onto the open', () => {
    const rows: WorkRow[] = [
      {
        tag: 'card',
        card: {
          id: 'm-1',
          row: 'arrived',
          peer: 'forge/steward',
          body: 'the summary line\nand the body alone',
          org: null,
          status: 'completed',
          ack: null,
          seat: null,
          seats: [],
        },
      },
    ];

    const app = mount(Leaves, { target: document.body, props: { rows } });
    try {
      flushSync();
      const card = document.querySelector('details.leaf');
      if (!(card instanceof HTMLDetailsElement)) throw new Error('the card row drew no details');
      expect(card.textContent ?? '', 'nothing under a closed card').not.toContain('the body alone');

      card.open = true;
      card.dispatchEvent(new Event('toggle'));
      flushSync();
      expect(card.textContent ?? '', 'and the body is drawn onto the open').toContain(
        'the body alone',
      );
    } finally {
      void unmount(app);
      document.body.innerHTML = '';
    }
  });
});
