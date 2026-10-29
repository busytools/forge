// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it } from 'vitest';

import Card from './Card.svelte';
import type { AnsweredQuestion } from './units';

let app: Record<string, unknown> | null = null;

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  document.body.innerHTML = '';
});

const pair = (over: Partial<AnsweredQuestion> = {}): AnsweredQuestion => ({
  question: 'Which colour?',
  state: 'answered',
  picked_labels: [],
  typed_note: null,
  ...over,
});

describe('the question card', () => {
  it('marks a question nobody answered, and draws no answer under it', () => {
    // The mark carries the word, not a tone alone: a cleared dock leaves the
    // card as the question's only trace, so it has to say why it is bare.
    app = mount(Card, {
      target: document.body,
      props: {
        asked: [
          pair({
            state: 'unanswered',
            // A pair marked unanswered holds no answer, and the card draws
            // none even if one arrives on it.
            picked_labels: ['Red'],
            typed_note: 'a teal, not listed',
          }),
        ],
      },
    });
    flushSync();

    const drawn = document.body.textContent ?? '';
    expect(drawn, 'the question itself').toContain('Which colour?');
    expect(drawn, 'the word the mark carries').toContain('unanswered');
    expect(drawn, 'no picked label under it').not.toContain('Red');
    expect(drawn, 'and no typed note').not.toContain('a teal');
  });

  it('draws an answered question as its picks and its note, unmarked', () => {
    app = mount(Card, {
      target: document.body,
      props: {
        asked: [pair({ picked_labels: ['Red'], typed_note: 'a teal, not listed' })],
      },
    });
    flushSync();

    const drawn = document.body.textContent ?? '';
    expect(drawn, 'the picks').toContain('Red');
    expect(drawn, 'the typed note').toContain('a teal, not listed');
    expect(drawn, 'no mark, because somebody answered').not.toContain('unanswered');
  });
});
