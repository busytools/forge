// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { describe, expect, it } from 'vitest';

import Procedure from './Procedure.svelte';

const PROCEDURE =
  'A handoff is starting for this session: forge will replace it with a new session.\n\n' +
  'Invoke the handoff skill and follow it.';

/** The row's own open, as MOUNTED DOM: closed it names itself, opened it reads. */
describe('the handoff procedure row, mounted', () => {
  it('names itself closed and holds the whole procedure behind its open', () => {
    const app = mount(Procedure, { target: document.body, props: { text: PROCEDURE } });
    try {
      flushSync();
      expect(document.body.textContent ?? '', 'closed, the row says what it is').toContain(
        'handoff procedure',
      );
      expect(document.body.textContent ?? '', 'and nothing of the procedure yet').not.toContain(
        'Invoke the handoff skill',
      );

      const row = document.querySelector('details');
      if (!(row instanceof HTMLDetailsElement)) throw new Error('the row drew no details');
      row.open = true;
      row.dispatchEvent(new Event('toggle'));
      flushSync();
      expect(document.body.textContent ?? '', 'opened, the whole text is reachable').toContain(
        'Invoke the handoff skill',
      );
      expect(document.body.textContent ?? '', 'and the opening line too').toContain(
        'A handoff is starting for this session',
      );
    } finally {
      void unmount(app);
      document.body.innerHTML = '';
    }
  });
});
