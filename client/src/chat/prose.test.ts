import { describe, expect, it } from 'vitest';

import { languageOf, renderCode } from './code';
import { renderProse } from './prose';

describe('markdown, as the maintained module renders it', () => {
  it('renders the shapes the prose of a conversation uses', () => {
    const html = renderProse('## a heading\n\n- one\n- two\n\n`code` and **bold**');

    expect(html).toContain('<h2>');
    expect(html).toContain('<li>one</li>');
    expect(html).toContain('<code>code</code>');
    expect(html).toContain('<strong>bold</strong>');
  });

  it('never lets raw HTML through into the page', () => {
    // A session's prose can quote anything it read - a page, a log, a file
    // from someone else's repository - and the page would run what it quotes.
    const html = renderProse('before <img src=x onerror="alert(1)"> after');

    expect(html).not.toContain('<img');
    expect(html).toContain('&lt;img');
  });

  it('refuses a link that is not a link', () => {
    // The rendered text keeps the source, which is what leaves it readable;
    // what must not happen is that it becomes an anchor for the page to
    // follow.
    expect(renderProse('[click](javascript:alert(1))')).not.toContain('href');
    expect(renderProse('[a page](https://example.com)')).toContain('href="https://example.com"');
  });
});

describe('a source file, as the maintained highlighter draws it', () => {
  it('takes the language from the path a call named', () => {
    expect(languageOf('crates/forge-server/src/family.rs')).toBe('rust');
    expect(languageOf('client/src/chat/Chat.svelte')).toBeNull();
    expect(languageOf('a/file.py')).toBe('python');
    expect(languageOf('no-extension')).toBeNull();
  });

  it('marks the tokens up in the classes the highlighter emits', () => {
    const html = renderCode('rust', 'pub fn main() { let x = "one"; }');

    expect(html).toContain('hljs-');
    expect(html).toContain('pub');
  });

  it('escapes a file it has no language for rather than dropping it', () => {
    // The page never invents text a reader cannot use, and it never lets a
    // file it cannot parse through as markup either.
    const html = renderCode('rust', 'let a = 1 < 2;');

    expect(renderCode(null, 'const a = <div>;')).toContain('&lt;div&gt;');
    expect(html).toContain('&lt;');
  });
});
