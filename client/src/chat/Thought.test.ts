import { render } from 'svelte/server';
import { describe, expect, it } from 'vitest';

import Thought from './Thought.svelte';

/**
 * The row one thinking block draws.
 *
 * **A closed row carries its summary and nothing else.** A thought's body is
 * markdown - headings, lists, code - and the summary already previews the
 * words, so the body is the rendering alone: on a long conversation the
 * thoughts are the biggest body source there is, and a closed row must not
 * carry their markup.
 */
const TEXT = '# Weighing it\n\nthe reasoning proper\n\n- one\n- two';

describe('a thought row', () => {
  it('carries summary markup only while it is closed', () => {
    const closed = render(Thought, { props: { text: TEXT } }).body;
    const under = closed.slice(closed.indexOf('</summary>'));
    expect(under, 'no heading under a closed row').not.toContain('<h1>');
    expect(under, 'no list either').not.toContain('<li>');

    const open = render(Thought, { props: { text: TEXT, open: true } }).body;
    expect(open, 'the heading is a heading once open').toContain('<h1>');
    expect(open, 'and the list is a list').toContain('<li>');
  });
});
