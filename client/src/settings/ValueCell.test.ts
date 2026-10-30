// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it } from 'vitest';

import ValueCell from './ValueCell.svelte';

/** The cells a value's shape draws, the empty forms included: a blank cell is
 *  indistinguishable from a value this client failed to draw. */
let app: Record<string, unknown> | null = null;

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  document.body.innerHTML = '';
});

describe('a value cell', () => {
  it('names an empty value rather than drawing a blank', () => {
    app = mount(ValueCell, {
      target: document.body,
      props: { value: { kind: 'string', text: '' } },
    });
    flushSync();
    expect(document.body.textContent?.trim(), 'an empty string reads none').toBe('none');
  });

  it('draws an unset client axis with the built-in it falls back to', () => {
    app = mount(ValueCell, {
      target: document.body,
      props: { value: { kind: 'unset' }, builtIn: 'panes' },
    });
    flushSync();
    expect(document.body.textContent, 'the built-in is display only').toContain('built-in panes');
  });
});
