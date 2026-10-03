// @vitest-environment jsdom
import axe from 'axe-core';
import { flushSync, mount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import type { Connection } from '../socket';
import type { QueuedPromptRow } from '../session/wire';
import type { SessionSlot } from '../wire/types';
import Queue from './Queue.svelte';
import { faceAt, walk } from './queue';

/**
 * The pile: the rows the core holds, the walk's arithmetic, and what this
 * view does with them.
 *
 * **What leaves the pile is the core's answer, not this view's**: a cancel
 * dispatches and the row stays until the stream settles it, which is why the
 * cancel tests assert the DISPATCH rather than an empty pile.
 */

const SLOT: SessionSlot = { org: 'Busytools', project: 'forge', label: 'lead' };

const row = (uuid: string): QueuedPromptRow => ({ uuid, source: 'you', text: uuid });

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
  // Inside a landmark: axe's `region` rule is about the page, and a bare body
  // with one component on it is a finding about this harness rather than
  // about the pile.
  const main = document.createElement('main');
  host = document.createElement('div');
  main.append(host);
  document.body.append(main);
});

afterEach(() => {
  document.body.innerHTML = '';
});

function draw(held: QueuedPromptRow[], connection: Connection): void {
  const app = mount(Queue, { target: host, props: { rows: held, slot: SLOT, connection } });
  flushSync();
  // The unmount rides the afterEach that empties the host.
  void app;
}

function pile(): HTMLElement {
  const found = host.querySelector<HTMLElement>('.pile [role="listbox"]');
  if (found === null) throw new Error('the pile did not draw');
  return found;
}

function key(target: HTMLElement, name: string): void {
  target.dispatchEvent(
    new KeyboardEvent('keydown', { key: name, bubbles: true, cancelable: true }),
  );
  flushSync();
}

describe('the walk', () => {
  const three = [row('oldest'), row('middle'), row('newest')];

  it('enters at the newest from the box, and stops on the first queued prompt', () => {
    expect(walk(null, three, 'up')).toBe('newest');
    expect(walk('newest', three, 'up')).toBe('middle');
    expect(walk('middle', three, 'up')).toBe('oldest');
    expect(walk('oldest', three, 'up'), 'the first queued prompt is where it stops').toBe('oldest');
  });

  it('comes back down and lands in the box past the newest', () => {
    expect(walk('oldest', three, 'down')).toBe('middle');
    expect(walk('newest', three, 'down'), 'past the newest is the box').toBeNull();
  });

  it('re-arms at the newest when the pointed row left', () => {
    // The cursor names a row the core settled under the walk: it is no longer
    // in the pile, and a walk that indexed into it would move nothing at all.
    expect(walk('delivered', three, 'up')).toBeNull();
    expect(walk('delivered', three, 'down')).toBeNull();
  });
});

describe('the face', () => {
  const three = [row('oldest'), row('middle'), row('newest')];

  it('is the cursor row when the walk is on it', () => {
    expect(faceAt('middle', three)).toBe(1);
  });

  it('is the newest when the walk is in the box, or on a row that has left', () => {
    expect(faceAt(null, three)).toBe(2);
    expect(faceAt('delivered', three)).toBe(2);
  });
});

describe('the queue pile', () => {
  it('draws nothing when nothing is waiting', () => {
    const { connection } = recording();
    draw([], connection);

    expect(host.querySelector('.pile')).toBeNull();
  });

  it('walks from the newest to the oldest and back into the box', () => {
    const { connection } = recording();
    draw(rows('one', 'two'), connection);

    key(pile(), 'ArrowUp');
    expect(
      pile().querySelector('.row.cur')?.getAttribute('id'),
      'the first up walks to the newest row',
    ).toBe('u1');

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
    expect(pile().querySelector('.row.cur')?.getAttribute('id'), 'down returns to the newest').toBe(
      'u1',
    );
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
      'the row leaves when the core says so, not when the key does',
    ).toHaveLength(1);
  });

  it('makes the depth by the step, so a deep queue costs steps and not rows', () => {
    const { connection } = recording();
    draw(rows('one', 'two', 'three', 'four'), connection);

    const stack = host.querySelector<HTMLElement>('.rows');
    expect(stack?.style.height, 'the face plus one step per older prompt').toBe(`${64 + 3 * 6}px`);

    const drawn = host.querySelectorAll<HTMLElement>('.row');
    expect(drawn[3]?.style.transform, 'the newest sits at the foot').toBe('translateY(0px)');
    expect(drawn[0]?.style.transform, 'the oldest is three steps above it').toBe(
      'translateY(-18px)',
    );
    expect(
      drawn[0]?.classList.contains('back'),
      'an older row is an edge: it keeps its words to itself',
    ).toBe(true);
  });

  it('says what an ending was, where the card was', () => {
    const { connection } = recording();
    mount(Queue, {
      target: host,
      props: {
        rows: [],
        ended: { text: 'the lost one', state: 'discarded' },
        slot: SLOT,
        connection,
      },
    });
    flushSync();

    expect(
      host.querySelector('.ended')?.textContent,
      'a row that vanishes silently reads as one that was delivered',
    ).toBe('discarded, never sent');
  });

  /**
   * The pile is the PR's one new interactive surface, and axe has to see it
   * WALKED: the walked markup is where the cursor row, its position and the
   * cancel control all exist, and the server render draws none of them.
   */
  it('walks with no axe violation over the walked markup', async () => {
    const { connection } = recording();
    draw(rows('one', 'two'), connection);
    key(pile(), 'ArrowUp');
    pile().focus();
    flushSync();

    const found = await axe.run(document.body);
    expect(found.violations.map((violation) => violation.id)).toEqual([]);
  });
});
