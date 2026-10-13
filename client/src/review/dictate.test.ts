/**
 * What a landed take means to the review page: whose words they are, and when
 * they have already been placed.
 */

import { describe, expect, it } from 'vitest';

import type { Notice } from '../composer/wire';
import { landed } from './dictate';

const words: Notice = { kind: 'landed', text: 'this read is the whole input', truncated: false };
const line: Notice = { kind: 'line', tone: 'bad', text: 'no input device is available' };

describe('a landed take', () => {
  it('places its words once', () => {
    expect(landed(words, true, null)).toBe('this read is the whole input');
    expect(
      landed(words, true, 'this read is the whole input'),
      'the same notice is not placed twice',
    ).toBeNull();
  });

  it('is not this page’s when it never watched the take', () => {
    // A page that attaches finds the previous take's notice still held; its
    // words are the box's only if this page saw the take that left them.
    expect(landed(words, false, null)).toBeNull();
  });

  it('places words only: a line about a take is not words for the box', () => {
    expect(landed(line, true, null)).toBeNull();
    expect(landed(null, true, null)).toBeNull();
  });
});
