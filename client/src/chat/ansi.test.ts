import { describe, expect, it } from 'vitest';

import { ansiRuns } from './ansi';

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
   * **Extended colour is the cut's own boundary**: `38;5;n` and `38;2;r;g;b`
   * are read as the sequence they are (so nothing leaks onto the page) and
   * leave the current colour alone. Nothing is invented for them.
   */
  it('keeps the current colour across an extended-colour sequence', () => {
    expect(ansiRuns('\u001b[31mr\u001b[38;5;208mx\u001b[0m')).toEqual([
      { text: 'rx', cls: 'ansi-red' },
    ]);
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
