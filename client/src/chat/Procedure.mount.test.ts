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

  it('keeps the brief one fact per line', () => {
    // The brief writes one `label: value` line per fact with single newlines.
    // Markdown soft-breaks those into one running paragraph, which reads the
    // seat, cwd, branch and role as prose - and this row is now the only
    // carrier of the handoff's information.
    const brief =
      'HANDOFF - a -> b\n\nPART 1: GROUND TRUTH\nseat: TestOrg/send-row/lead\ncwd: /tmp/proj\nbranch: master';
    const app = mount(Procedure, { target: document.body, props: { text: brief, open: true } });
    try {
      flushSync();
      const breaks = document.querySelectorAll('.prose br');
      expect(breaks.length, 'each fact line keeps its own break').toBeGreaterThanOrEqual(2);
      expect(document.body.textContent, 'and both facts are on the page').toContain(
        'cwd: /tmp/proj',
      );
    } finally {
      void unmount(app);
      document.body.innerHTML = '';
    }
  });
});
