import { render } from 'svelte/server';
import { describe, expect, it } from 'vitest';

import Field from './Field.svelte';

/** The box, drawn, with whatever the test gives it. */
function draw(props: Record<string, unknown>): string {
  return render(Field, {
    props: { editor: 'composer', value: '', placeholder: 'p', onkeydown: () => {}, ...props },
  }).body;
}

describe('the one field every surface uses', () => {
  it('renders a textarea carrying the value and the placeholder', () => {
    const body = draw({ value: 'hello', placeholder: 'Type a message...' });
    expect(body, 'the field draws a real textarea').toContain('<textarea');
    expect(body, 'the value is on it').toContain('hello');
    expect(body, 'the placeholder is on it').toContain('Type a message...');
  });

  it('points at a list only when it is given one', () => {
    expect(
      draw({ aria: { controls: 'list-1' } }),
      'a list is announced when there is one',
    ).toContain('aria-controls="list-1"');
    expect(draw({}), 'and not announced when there is not').not.toContain('aria-controls');
  });

  it('keeps the classes the caller needs, because the sheet is the sheet', () => {
    // The dock's box is styled by `.dock .notes` and the composer's by
    // `.line .txt`; a field that painted its own class would drop both.
    expect(draw({ class: 'notes' }), "the caller's class is on the element").toContain('notes');
  });

  it("draws the element kind its surface needs, with that element's own attributes", () => {
    // The connect screen is a single-line box: it connects on Enter, which a
    // textarea does not do, and its own sheet styles `input`, `input:focus-visible`
    // and `input[aria-invalid='true']`, none of which match a textarea.
    const body = draw({
      editor: 'connect',
      element: 'input',
      placeholder: '127.0.0.1:8790',
      aria: { invalid: true, describedBy: 'why' },
      id: 'address',
      name: 'address',
      autocapitalize: 'off',
    });
    expect(body, 'a single-line surface gets an input').toContain('<input');
    expect(body, 'and not a textarea').not.toContain('<textarea');
    expect(body, 'the label still finds it').toContain('id="address"');
    expect(body, 'the field is still named').toContain('name="address"');
    expect(body, 'the error state still reaches it').toContain('aria-invalid="true"');
    expect(body, 'so does what explains it').toContain('aria-describedby="why"');
  });
});
