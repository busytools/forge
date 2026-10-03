// @vitest-environment jsdom
import { flushSync, mount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import type { Connection } from '../socket';
import type { QueuedPromptRow } from '../session/wire';
import type { SessionSlot } from '../wire/types';
import Queue from './Queue.svelte';

/**
 * The pile: the rows the core holds, and the two things this view does with
 * them - walk and cancel.
 *
 * **What leaves the pile is the core's answer, not this view's**: a cancel
 * dispatches and the row stays until the stream settles it, which is why the
 * cancel test asserts the DISPATCH rather than an empty pile.
 */

const SLOT: SessionSlot = { org: 'Busytools', project: 'forge', label: 'lead' };

const rows = (...texts: string[]): QueuedPromptRow[] =>
  texts.map((text, at) => ({ uuid: `u${at}`, source: 'you', text }));

/** A connection that records what the component dispatched. */
function recording(): { sent: unknown[]; connection: Connection } {
  const sent: unknown[] = [];
  const connection = {
    dispatch: (command: unknown) => {
      sent.push({ command });
    },
  } as unknown as Connection;
  return { sent, connection };
}

let host: HTMLElement;

beforeEach(() => {
  host = document.createElement('div');
  document.body.append(host);
});

afterEach(() => {
  document.body.innerHTML = '';
});

function draw(held: QueuedPromptRow[], connection: Connection): void {
  const app = mount(Queue, { target: host, props: { rows: held, slot: SLOT, connection } });
  flushSync();
  // The unmount rides the afterEach that empties the host; keeping a teardown
  // here would need one per test and nothing else needs it.
  void app;
}

function pile(): HTMLElement {
  const found = host.querySelector<HTMLElement>('.pile');
  if (found === null) throw new Error('the pile did not draw');
  return found;
}

function key(target: HTMLElement, name: string): void {
  target.dispatchEvent(new KeyboardEvent('keydown', { key: name, bubbles: true, cancelable: true }));
  flushSync();
}

describe('the queue pile', () => {
  it('draws nothing when nothing is waiting', () => {
    const { connection } = recording();
    draw([], connection);

    expect(host.querySelector('.pile')).toBeNull();
  });

  it('walks back from the newest row and off the end into the box', () => {
    const { connection } = recording();
    draw(rows('one', 'two'), connection);

    key(pile(), 'ArrowUp');
    expect(pile().querySelector('.row.cur'), 'the first up walks to the newest row').not.toBeNull();

    key(pile(), 'ArrowUp');
    expect(pile().querySelector('.row.cur')?.getAttribute('id'), 'the second walks older').toBe(
      'u0',
    );

    key(pile(), 'ArrowUp');
    expect(
      pile().querySelector('.row.cur')?.getAttribute('id'),
      'up stops on the first queued prompt',
    ).toBe('u0');

    key(pile(), 'ArrowDown');
    expect(
      pile().querySelector('.row.cur')?.getAttribute('id'),
      'down from the box returns to the newest',
    ).toBe('u1');
    key(pile(), 'ArrowDown');
    expect(pile().querySelector('.row.cur'), 'down past the newest lands in the box').toBeNull();
  });

  it('dispatches a cancel for the row it is on, and leaves the row to the stream', () => {
    const { sent, connection } = recording();
    draw(rows('one'), connection);

    key(pile(), 'ArrowUp');
    key(pile(), 'Backspace');

    expect(sent).toEqual([{ command: { cancel_queued_prompt: { key: SLOT, uuid: 'u0' } } }]);
    expect(
      pile().querySelectorAll('.row'),
      'the row leaves when the core says so, not when the click does',
    ).toHaveLength(1);
  });

  it('keeps the words of the drawn row to itself, and arcs the ones behind', () => {
    const { connection } = recording();
    draw(rows('one', 'two'), connection);

    const drawn = host.querySelectorAll<HTMLElement>('.row');
    expect(drawn).toHaveLength(2);
    // The newest sits at the bottom against the box; the older is one arc up.
    expect(drawn[1]?.classList.contains('back'), 'the newest row is not an arc').toBe(false);
    expect(drawn[0]?.classList.contains('back'), 'an older row is one arc').toBe(true);
  });
});
