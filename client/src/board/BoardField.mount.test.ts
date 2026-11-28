// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it } from 'vitest';

import type { Dictation } from '../composer/dictate.svelte';
import BoardField from './BoardField.svelte';
import type { BoardFieldHandle, BoardVoice } from './voice';

/**
 * The board's text box: plain when the page has no dictation, and wearing
 * the composer's own mic, take bar and landing when it has. A box without a
 * mic is the defect this pins against.
 */
let frame: Record<string, unknown> | null = null;

afterEach(async () => {
  if (frame !== null) await unmount(frame);
  frame = null;
  document.body.innerHTML = '';
});

/** A Dictation stub: the pages' real object is driven by a live socket. */
function dictateStub(): Dictation {
  return {
    take: null,
    held: false,
    panel: false,
    toggle: () => {},
    act: () => {},
  } as unknown as Dictation;
}

function voiceOver(
  registered: Map<string, BoardFieldHandle>,
  over: Partial<BoardVoice> = {},
): BoardVoice {
  return {
    dictate: dictateStub(),
    activeId: null,
    take: null,
    phase: null,
    wire: null,
    silent: null,
    line: null,
    lineId: null,
    press: () => {},
    cancel: () => {},
    register: (id, box) => {
      registered.set(id, box);
      return () => registered.delete(id);
    },
    ...over,
  };
}

describe('the board text box', () => {
  it('is a plain input when the page has no dictation', () => {
    frame = mount(BoardField, {
      target: document.body,
      props: { id: 'a1:note', placeholder: 'your answer', label: 'your answer', value: '' },
    });
    flushSync();
    expect(document.querySelector('input'), 'the field is not drawn').not.toBeNull();
    expect(document.querySelector('.mic'), 'a mic drew with no take to run').toBeNull();
  });

  it('wears the mic, and a landing puts its words in the box', () => {
    const registered = new Map<string, BoardFieldHandle>();
    let written = '';
    frame = mount(BoardField, {
      target: document.body,
      props: {
        id: 'a1:note',
        placeholder: 'your answer',
        label: 'your answer',
        value: '',
        voice: voiceOver(registered),
        oninput: (text: string) => {
          written = text;
        },
      },
    });
    flushSync();
    expect(document.querySelector('.mic'), 'the box carries no dictation control').not.toBeNull();
    const box = registered.get('a1:note');
    if (box === undefined) throw new Error('the box did not register its landing target');
    box.set('the scratch store');
    expect(written, 'the landed words did not reach the box').toBe('the scratch store');
  });

  it('draws the take bar in the field it began in, not beside it', () => {
    const registered = new Map<string, BoardFieldHandle>();
    frame = mount(BoardField, {
      target: document.body,
      props: {
        id: 'a1:note',
        placeholder: 'your answer',
        label: 'your answer',
        value: '',
        voice: voiceOver(registered, { activeId: 'a1:note', phase: 'recording' }),
      },
    });
    flushSync();
    expect(document.querySelector('input'), 'the field and the bar drew together').toBeNull();
    expect(document.querySelector('.mic'), 'the mic left with the field').not.toBeNull();
  });

  it('names a failed take in the box that tried it', () => {
    const registered = new Map<string, BoardFieldHandle>();
    frame = mount(BoardField, {
      target: document.body,
      props: {
        id: 'a1:note',
        placeholder: 'your answer',
        label: 'your answer',
        value: '',
        voice: voiceOver(registered, {
          line: 'the microphone could not open',
          lineId: 'a1:note',
        }),
      },
    });
    flushSync();
    expect(document.body.textContent).toContain('the microphone could not open');
  });
});
