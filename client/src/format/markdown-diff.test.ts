// @vitest-environment jsdom
/**
 * The rendered markdown diff's block rules: a promoted heading renders as a
 * heading, a removed fence stays a code block with its body struck, a table
 * row keeps its cells, and a link destination never carries a sentinel.
 */
import { describe, expect, it } from 'vitest';

import { renderProse } from '../chat/prose';
import type { SourceLine } from './diff';
import { hasDefinitionChange, markdownDiffHtml, plain } from './markdown-diff';

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

  /**
   * **A fence later in a removed run still opens its block.** The marker is
   * a boundary; wrapping it as a removed line would leave the body parsed as
   * markdown and the sentinels crossing `<code>`.
   */
  it('opens a fence that is not the first line of a removed run', () => {
    const drawn = html([
      { kind: 'del', text: 'an intro line' },
      { kind: 'del', text: '```rust' },
      { kind: 'del', text: 'fn main() {}' },
      { kind: 'del', text: '```' },
    ]);

    expect(drawn.querySelector('.code'), 'the block opens').not.toBeNull();
    const struck = [...drawn.querySelectorAll('.mddel')].map((span) => span.textContent);
    expect(struck, 'the intro and the code body are struck separately').toEqual([
      'an intro line',
      'fn main() {}',
    ]);
  });

  /**
   * **A whole-fence replacement is a close and an open**, not a del closing
   * marker paired with the add opening marker: the new language must act as
   * a new block, and no literal marker may land inside the old one.
   */
  /**
   * **A marker whose text alone changed is ONE marker.** A fence's info
   * string change is an adjacent removed/added pair with no body between
   * them; drawing both toggles the block twice and the new opener lands as
   * rust code content.
   */
  it('draws an info-string-only fence change as one block in the new language', () => {
    const drawn = html([
      { kind: 'del', text: '```rust' },
      { kind: 'add', text: '```python' },
      { kind: 'ctx', text: 'fn main() {}' },
      { kind: 'ctx', text: '```' },
    ]);

    const languages = [...drawn.querySelectorAll('.code .lang')].map((node) => node.textContent);
    expect(languages, 'the block opens in the new language').toEqual(['python']);
    expect(drawn.textContent ?? '', 'no literal marker in the code').not.toContain('```');
  });

  /** A closing delimiter change is the same marker-only rule at the close. */
  it('draws a changed closing fence as one balanced block', () => {
    // The unchanged opener is `~~~`, so the added `~~~` is the valid close
    // of that block (a ``` opener could not be closed by a ~~~ marker).
    const drawn = html([
      { kind: 'ctx', text: '~~~rust' },
      { kind: 'ctx', text: 'fn main() {}' },
      { kind: 'del', text: '```' },
      { kind: 'add', text: '~~~' },
    ]);

    const languages = [...drawn.querySelectorAll('.code .lang')].map((node) => node.textContent);
    expect(languages, 'one block, in its own language').toEqual(['rust']);
    expect(drawn.textContent ?? '', 'no literal closing marker').not.toContain('~~~');
  });

  it('draws a rust fence replaced by a python fence as two blocks', () => {
    const drawn = html([
      { kind: 'del', text: '```rust' },
      { kind: 'del', text: 'fn main() {}' },
      { kind: 'del', text: '```' },
      { kind: 'add', text: '```python' },
      { kind: 'add', text: 'print(1)' },
      { kind: 'add', text: '```' },
    ]);

    const languages = [...drawn.querySelectorAll('.code .lang')].map((node) => node.textContent);
    expect(languages, 'the old block and the new one, in that order').toEqual(['rust', 'python']);
    const struck = [...drawn.querySelectorAll('.code .mddel')].map((span) => span.textContent);
    expect(struck, 'the removed body stays struck in its own block').toEqual(['fn main() {}']);
    expect(drawn.textContent ?? '', 'no literal marker leaked').not.toContain('```');
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

describe('a reference-style link definition', () => {
  /**
   * **A definition change goes to the source diff.** A rendered view cannot
   * show a definition, and a sentinel inside one breaks the definition and
   * every reference to it.
   */
  it('is detected as a change the rendered diff cannot draw', () => {
    expect(
      hasDefinitionChange([
        { kind: 'del', text: '[docs]: https://example.com/old' },
        { kind: 'add', text: '[docs]: https://example.com/new' },
      ]),
    ).toBe(true);
    expect(
      hasDefinitionChange([{ kind: 'ctx', text: '[docs]: https://example.com/old' }]),
      'an unchanged definition draws no change',
    ).toBe(false);
  });

  /** A definition-shaped line inside a fence is code, not a definition. */
  it('does not fire on a definition-shaped line inside fenced code', () => {
    expect(
      hasDefinitionChange([
        { kind: 'ctx', text: '```md' },
        { kind: 'del', text: '[docs]: https://example.com' },
        { kind: 'ctx', text: '```' },
      ]),
    ).toBe(false);
  });

  /** A destination on the following indented line is the same definition. */
  it('detects a definition whose destination is on the next line', () => {
    expect(hasDefinitionChange([{ kind: 'del', text: '[docs]:' }])).toBe(true);
  });
});

describe('the sentinel-clearing pass', () => {
  /**
   * **Every spelling of a sentinel is cleared.** A renderer percent-encodes
   * a sentinel that reaches an attribute; an identity pass would leak it
   * into an href.
   */
  it('clears literal, encoded and lowercase-encoded sentinels', () => {
    expect(plain('a\u{E000}b')).toBe('ab');
    expect(plain('a%EE%80%80b')).toBe('ab');
    expect(plain('a%ee%80%80b')).toBe('ab');
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
