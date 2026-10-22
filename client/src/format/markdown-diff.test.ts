// @vitest-environment jsdom
/**
 * The rendered markdown diff's block rules: a promoted heading renders as a
 * heading, a removed fence stays a code block with its body struck, a table
 * row keeps its cells, and a link destination never carries a sentinel.
 */
import { describe, expect, it } from 'vitest';

import { renderProse } from '../chat/prose';
import type { SourceLine } from './diff';
import { markdownDiffHtml } from './markdown-diff';

const html = (lines: SourceLine[]): HTMLElement => {
  const host = document.createElement('div');
  host.innerHTML = markdownDiffHtml(lines, renderProse);
  return host;
};

describe('a markdown change across block markers', () => {
  it('renders a promoted paragraph as a heading, with the old line struck', () => {
    const drawn = html([
      { kind: 'del', text: 'A short intro' },
      { kind: 'add', text: '## A short intro' },
    ]);

    expect(drawn.querySelector('h2')?.textContent, 'the heading parses').toBe('A short intro');
    expect(drawn.querySelector('.mddel')?.textContent, 'the old line is kept, struck').toBe(
      'A short intro',
    );
    expect(drawn.querySelector('.mdadd')?.textContent).toBe('A short intro');
  });

  it('keeps a removed fence a code block, its body struck', () => {
    const drawn = html([
      { kind: 'del', text: '```rust' },
      { kind: 'del', text: 'fn main() {}' },
      { kind: 'del', text: '```' },
    ]);

    expect(drawn.querySelector('.code'), 'the fence still draws a code panel').not.toBeNull();
    const struck = drawn.querySelector('.mddel');
    expect(struck?.textContent, 'the body is struck inside the block').toBe('fn main() {}');
    expect(
      drawn.querySelector('p')?.textContent ?? '',
      'no fence marker leaked as prose',
    ).not.toContain('```');
  });

  it('keeps a changed table a table, each changed cell wrapped on its own', () => {
    const drawn = html([
      { kind: 'ctx', text: '| a | b |' },
      { kind: 'ctx', text: '| - | - |' },
      { kind: 'del', text: '| old | keep |' },
      { kind: 'add', text: '| new | keep |' },
    ]);

    expect(drawn.querySelector('table'), 'the table still parses').not.toBeNull();
    const struck = [...drawn.querySelectorAll('.mddel')].map((cell) => cell.textContent?.trim());
    const added = [...drawn.querySelectorAll('.mdadd')].map((cell) => cell.textContent?.trim());
    expect(struck).toEqual(['old', 'keep']);
    expect(added).toEqual(['new', 'keep']);
  });
});

describe('a change inside a link destination', () => {
  it('draws the two whole lines and leaves no sentinel in an href', () => {
    const drawn = html([
      { kind: 'del', text: 'See [the docs](https://example.com/old).' },
      { kind: 'add', text: 'See [the docs](https://example.com/new).' },
    ]);

    const links = [...drawn.querySelectorAll('a')];
    expect(links.length, 'both sides draw a link').toBe(2);
    expect(links.map((link) => link.getAttribute('href'))).toEqual([
      'https://example.com/old',
      'https://example.com/new',
    ]);
    expect(drawn.innerHTML, 'no literal or encoded sentinel in the DOM').not.toMatch(
      /[\u{E000}\u{E001}\u{E002}\u{E003}]|%EE%80%8[0-3]/iu,
    );
    expect(drawn.querySelector('.mddel'), 'the old link is struck').not.toBeNull();
  });
});
