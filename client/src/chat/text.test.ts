import { describe, expect, it } from 'vitest';

import { firstLine, headline, searchHits, shortPath, stripEscapes } from './text';

describe('the text a command produced, as a page draws it', () => {
  it('strips the colour a command wrote', () => {
    expect(stripEscapes('\u{1b}[31mred\u{1b}[0m and \u{1b}[1;32mgreen\u{1b}[m')).toBe(
      'red and green',
    );
  });

  it('strips a sequence that never terminated', () => {
    // A truncated sequence is the case the browser does not save you from:
    // what it leaves on screen is the parameters themselves, and the reader
    // gets `38;5;12m the rest of the line` where the tool meant to print text.
    expect(stripEscapes('running\u{1b}[38;5;12')).toBe('running');
    expect(stripEscapes('done\u{1b}[0m\u{1b}]8;;http://x')).toBe('done');
  });

  it('strips the notifications and hyperlinks a tool emitted', () => {
    expect(stripEscapes('\u{1b}]777;notify;forge;finished\u{7}after')).toBe('after');
    expect(stripEscapes('\u{1b}]8;;http://example.com\u{1b}\\a link\u{1b}]8;;\u{1b}\\')).toBe(
      'a link',
    );
  });

  it('keeps the text a terminal would have kept', () => {
    // Line breaks and tabs survive; a carriage return does not, because it is
    // a redraw instruction rather than a character, and the page is not a
    // terminal to obey it.
    expect(stripEscapes('first\nsecond\tthird')).toBe('first\nsecond\tthird');
    expect(stripEscapes('progress\rfinished')).toBe('progressfinished');
    expect(stripEscapes('│ ├── \u{2713} done')).toBe('│ ├── \u{2713} done');
  });

  it('splits a search result into a location and the line it matched', () => {
    // Two parts, because the design's single string with a newline in it only
    // becomes two lines where something has already made the box
    // pre-formatted - and nothing has, so the newline collapses and the hit
    // draws as one line with its location run into its text.
    expect(
      searchHits('crates/forge-server/src/grouping.rs:142:  render_group_summary(unit, width)'),
    ).toEqual([
      {
        path: 'crates/forge-server/src/grouping.rs',
        line: '142',
        src: 'render_group_summary(unit, width)',
      },
    ]);

    expect(searchHits('a.rs:1:one\na.rs:2:two')).toHaveLength(2);
  });

  it('leaves a body that is not a set of hits as the text it is', () => {
    expect(searchHits('    Starting 42 tests across 3 binaries')).toBeNull();
    expect(searchHits('a.rs:1:one\nand a line that is not a hit')).toBeNull();
    expect(searchHits('   ')).toBeNull();
  });

  it('takes the first line that says anything', () => {
    expect(firstLine('\n\n  the real line  \nsecond')).toBe('the real line');
    expect(firstLine('   ')).toBe('');
  });

  it('names a call the way its row draws it', () => {
    expect(headline('Read', { file_path: '/Users/ved/Projects/forge/src/lib.rs' })).toBe(
      '/Users/ved/Projects/forge/src/lib.rs',
    );
    expect(headline('Bash', { command: 'just check', description: 'run the gate' })).toBe(
      'run the gate',
    );
    expect(headline('Grep', { pattern: 'render_group_summary', path: 'crates' })).toBe(
      'render_group_summary',
    );
    expect(headline('WebFetch', { url: 'https://docs.rs/virtua' })).toBe('https://docs.rs/virtua');
    expect(headline('mcp__forge__agents__list', {})).toBe('mcp__forge__agents__list');
  });

  it('drops the working directory a reader is already in', () => {
    expect(shortPath('/Users/ved/Projects/forge/crates/a.rs', '/Users/ved/Projects/forge')).toBe(
      'crates/a.rs',
    );
    expect(shortPath('/etc/hosts', '/Users/ved/Projects/forge')).toBe('/etc/hosts');
    expect(shortPath('crates/a.rs', null)).toBe('crates/a.rs');
  });
});
