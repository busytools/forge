import { describe, expect, it } from 'vitest';

import { fenceLanguage, languageOf, renderCode } from './code';
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

describe('a code block in a message', () => {
  it('draws an indented block as the same panel, with no language to name it', () => {
    // Four spaces is a code block with no fence and so no info string, and the
    // panel is what a block of code draws as here.
    const html = renderProse('    seen as code\n');

    expect(html).toContain('<div class="code">');
    expect(html).not.toContain('class="lang"');
  });

  it("draws as the page's own code panel, labelled by its info string", () => {
    const html = renderProse('```yaml\nkey: value\n```');

    expect(html).toContain('<div class="code">');
    expect(html).toContain('<div class="lang">yaml</div>');
    expect(html).not.toContain('```');
  });

  it('draws no label row for a fence that carries no info string', () => {
    // The terminal labels a panel whenever the fence carries one and draws no
    // row when it does not, so a bare fence must not invent a word for it.
    const html = renderProse('```\nplain text\n```');

    expect(html).toContain('<div class="code">');
    expect(html).not.toContain('class="lang"');
  });

  it('colours the fence through the same lookup a read body uses', () => {
    expect(renderProse('```rust\npub fn main() {}\n```')).toContain('hljs-');
    expect(renderProse('```toml\n[accounts]\n```')).toContain('hljs-');
  });

  it('escapes a fence whose body is markup rather than letting it through', () => {
    // The panel is spliced in as HTML, so a quoted file, log or page must
    // still arrive as text.
    expect(renderProse('```\n<script>x</script>\n```')).not.toContain('<script');
  });

  it('escapes the label rather than drawing the info string as markup', () => {
    // The info string is the model's own text and reaches the page inside an
    // element the panel builds.
    expect(renderProse('```<b>x\ncode\n```')).toContain('<div class="lang">&lt;b&gt;x</div>');
  });
});

describe('the language a fence names', () => {
  it("comes from the same table the paths use, on the info string's first word", () => {
    expect(fenceLanguage('rust')).toBe('rust');
    expect(fenceLanguage('toml')).toBe('ini');
    expect(fenceLanguage('  yaml  ')).toBe('yaml');
    // A fence's info string carries attributes after the language.
    expect(fenceLanguage('rust title="x"')).toBe('rust');
    expect(fenceLanguage('klingon')).toBeNull();
    expect(fenceLanguage('')).toBeNull();
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
