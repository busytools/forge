import { describe, expect, it } from 'vitest';

import { ansiRuns } from './ansi';
import { stripEscapes } from './text';

describe('the command output a box draws', () => {
  /** Plain output is one run with no style of its own: the box's own tone. */
  it('leaves text with no escapes as one unstyled run', () => {
    expect(ansiRuns('Compiling forge-server v1.1.19\n')).toEqual([
      { text: 'Compiling forge-server v1.1.19\n', cls: '' },
    ]);
  });

  /**
   * The point of the whole exercise: a colour a command wrote is a class on
   * the run, the theme's own token rather than the command's raw byte.
   */
  it('turns a colour into the theme class for it, up to the reset', () => {
    expect(ansiRuns('\u001b[31mred\u001b[0m plain')).toEqual([
      { text: 'red', cls: 'ansi-red' },
      { text: ' plain', cls: '' },
    ]);
  });

  /** A bright colour and an attribute ride together, in a fixed order. */
  it('reads bright colours and attributes together', () => {
    expect(ansiRuns('\u001b[1;92mok\u001b[0m')).toEqual([
      { text: 'ok', cls: 'ansi-bright-green ansi-bold' },
    ]);
  });

  /** A reset is the default state, however it is spelled. */
  it('reads a bare reset as the default style', () => {
    expect(ansiRuns('\u001b[33my\u001b[m z')).toEqual([
      { text: 'y', cls: 'ansi-yellow' },
      { text: ' z', cls: '' },
    ]);
  });

  /** `22` turns off bold and dim together, which is what the terminal does. */
  it('reads 22 as bold and dim off', () => {
    expect(ansiRuns('\u001b[1m\u001b[2mboth\u001b[22m plain')).toEqual([
      { text: 'both', cls: 'ansi-bold ansi-dim' },
      { text: ' plain', cls: '' },
    ]);
  });

  /**
   * The sequences this page cannot draw as style are dropped, never printed:
   * an erase, a cursor move, a window title. That is the same rule the strip
   * carried - a browser has no terminal to obey them.
   */
  it('drops the sequences it cannot draw as style', () => {
    expect(ansiRuns('a\u001b[2Jb\u001b]0;title\u0007c\u001b[1;1Hd')).toEqual([
      { text: 'abcd', cls: '' },
    ]);
  });

  /**
   * **Extended colour is the cut's own boundary, and its payload is a unit.**
   * `38;5;n` and `38;2;r;g;b` are read as the one sequence they are (so
   * nothing leaks onto the page) and leave the current colour alone. The
   * numbers must never reach the arms: `38;5;1` would set bold, `38;5;0`
   * would reset, and `38;2;31;31;31` would paint red - so each case is pinned
   * on a payload whose numbers collide with real ones.
   */
  it('keeps the current colour across an extended-colour sequence', () => {
    expect(ansiRuns('\u001b[31mr\u001b[38;5;1mx\u001b[0m')).toEqual([
      { text: 'rx', cls: 'ansi-red' },
    ]);
    expect(ansiRuns('\u001b[31mr\u001b[38;5;0mx\u001b[0m')).toEqual([
      { text: 'rx', cls: 'ansi-red' },
    ]);
    expect(ansiRuns('\u001b[31mr\u001b[38;2;31;31;31mx\u001b[0m')).toEqual([
      { text: 'rx', cls: 'ansi-red' },
    ]);
    // `48` is the background half and `58` the underline half, consumed the
    // same way - `58` was the one that still leaked its payload.
    expect(ansiRuns('\u001b[31mr\u001b[48;5;2mx\u001b[0m')).toEqual([
      { text: 'rx', cls: 'ansi-red' },
    ]);
    expect(ansiRuns('\u001b[31mr\u001b[58;5;1mx\u001b[0m')).toEqual([
      { text: 'rx', cls: 'ansi-red' },
    ]);
    expect(ansiRuns('\u001b[31mr\u001b[58;2;31;31;31mx\u001b[0m')).toEqual([
      { text: 'rx', cls: 'ansi-red' },
    ]);
  });

  /**
   * **The two scanners read the bytes the same way.** `ansiRuns` keeps the
   * text and `stripEscapes` throws it away, and the one property that must
   * hold across every shape - colour codes, string controls, truncated
   * sequences, intermediate-byte escapes, controls - is that they agree on
   * what the text IS. A drift here is a sequence one draws as residue and the
   * other drops.
   */
  it('keeps exactly the text the stripper keeps, over the shapes both read', () => {
    const corpus = [
      'plain text with no escapes',
      '\u001b[31mred\u001b[0m and \u001b[1;32mgreen\u001b[m',
      '\u001b[31mred\u001b[38;5;208mextended\u001b[0m',
      'running\u001b[38;5;12',
      'done\u001b[0m\u001b]8;;http://x',
      '\u001b]777;notify;forge;finished\u0007after',
      '\u001b]8;;http://example.com\u001b\\a link\u001b]8;;\u001b\\',
      'before \u001bPq~xyz\u001b\\ after',
      '\u001bXhello\u001b\\after',
      '\u001b^meta\u001b\\after',
      '\u001b_app\u001b\\after',
      'text\u001bPq~payload',
      '\u001b_a\u0007after',
      'before \u001b(B\u001b[m after',
      '\u001b)0plain',
      '\u001b#8',
      'first\nsecond\tthird',
      'progress\rfinished',
      '\u007fdelete\u0008back',
      // The CSI abort arm: a byte that cannot be part of a sequence, and a
      // second escape inside one. Both end the sequence where the stripper
      // ends it, which is the arm a corpus of valid sequences never reaches.
      '\u001b[31éx',
      '\u001b[31\u001b[0my',
      // And the four other arms a corpus of ordinary sequences misses: the
      // escape-and-one-character arm, an intermediate byte inside a CSI, the
      // escape-with-intermediate arm's edge (nothing after the introducer),
      // and the `~` final byte.
      '\u001b7saved',
      '\u001b[ 1mtext',
      '\u001b(',
      '\u001b[1~home',
    ];
    for (const text of corpus) {
      expect(
        ansiRuns(text)
          .map((run) => run.text)
          .join(''),
        `the runs' text is the stripper's text for ${JSON.stringify(text)}`,
      ).toBe(stripEscapes(text));
    }
  });

  /**
   * Control bytes a terminal acts on are dropped the way the strip dropped
   * them: a carriage return is an instruction to overwrite a line this box is
   * not drawing line by line. Tab and newline are layout, so they stay.
   */
  it('keeps layout bytes and drops the rest of the controls', () => {
    expect(ansiRuns('a\rb\tc\nd\u0007e')).toEqual([{ text: 'ab\tc\nde', cls: '' }]);
  });

  /** Runs with the same style coalesce rather than drawing an element each. */
  it('coalesces runs of one style into one', () => {
    expect(ansiRuns('\u001b[32ma\u001b[32mb\u001b[0mc')).toEqual([
      { text: 'ab', cls: 'ansi-green' },
      { text: 'c', cls: '' },
    ]);
  });

  /** Unterminated sequences eat what follows them, as they always have. */
  it('drops an unterminated sequence and what follows it', () => {
    expect(ansiRuns('before\u001b[31')).toEqual([{ text: 'before', cls: '' }]);
  });
});
