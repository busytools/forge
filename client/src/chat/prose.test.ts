import { readFileSync } from 'node:fs';

import { describe, expect, it } from 'vitest';

import { fenceLanguage, languageOf, renderCode } from './code';
import { renderInlineProse, renderProse } from './prose';

const SHEET = readFileSync(new URL('../assets/web.css', import.meta.url), 'utf8');

/**
 * Whether any rule the sheet writes for `unit` carries `said`.
 *
 * Scoped on `.prose` alone: prose draws in the work a turn did and in what the
 * reader typed, which the view this client replaced folded as a document too.
 */
function declares(unit: string, said: string): boolean {
  const rules = SHEET.matchAll(new RegExp(`\\.prose [^{}]*\\b${unit}\\b[^{}]*\\{([^}]*)\\}`, 'g'));
  return [...rules].some((rule) => rule[1]?.includes(said) ?? false);
}

describe('the markdown a message carries, as the sheet draws it', () => {
  it('configures the two renderers as one, apart from soft breaks', () => {
    // The two differ by their options and nothing else. A rule attached to one
    // instance and not the other is the hand-kept mirror this shape exists to
    // avoid - a fence drawn as the page's panel in prose and as the module's own
    // `<pre><code>` in a prompt is what that costs - so the pair is compared by
    // what they render rather than by how they were built: every markdown shape
    // the page draws, through both, byte for byte.
    const document = [
      '# a heading',
      '',
      'a paragraph with `code` and a [link](https://example.test).',
      '',
      '![a picture](https://example.test/x.png)',
      '',
      '- one',
      '- two',
      '',
      '> a quote',
      '',
      '```sh',
      'just check',
      '```',
      '',
      '    an indented block',
      '',
      '| a | b |',
      '| - | - |',
      '| 1 | 2 |',
      '',
      '---',
      '',
      'first line',
      'second line',
    ].join('\n');

    const prose = renderProse(document);
    const prompt = renderProse(document, true);

    expect(prompt).toContain('<br>');
    // The tag alone is the difference: the newline it replaces is still there,
    // so what is left of the prompt's render is the prose render verbatim.
    expect(prompt.replace(/<br>/g, ''), 'the split is the only difference').toBe(prose);
  });

  it('keeps a prompt newline as a break, where the same text joins for prose', () => {
    // The split the view this client replaced kept: its user path preserved
    // newlines and its assistant path did not - so a prompt's newlines survive
    // and an assistant's wrapped line joins back into one. A prompt is usually
    // several lines, so this is the shape a person meets first.
    const typed = 'first line\nsecond line';

    expect(renderProse(typed), 'assistant prose joins its soft break').not.toContain('<br>');
    expect(renderProse(typed, true), 'and a prompt keeps it').toContain('<br>');
  });

  it('draws every heading level as one bold line at the prose size', () => {
    // A sheet edit that drops a level is silent: it keeps drawing, only as the
    // browser's default against a reset that zeroes margins, which is an h1 at
    // twice the prose size and an h4 at two thirds of it.
    //
    // 500 rather than 700 because the vendored face ships Regular and Medium
    // only, so 700 was synthesized rather than drawn - see `density.test.ts`,
    // which holds every prose mark to a weight the face can render.
    for (const level of ['h1', 'h2', 'h3', 'h4', 'h5', 'h6']) {
      expect(declares(level, 'font-size: var(--fs-prose)'), `${level}: one size`).toBe(true);
      expect(declares(level, 'font-weight: 500'), `${level}: bold`).toBe(true);
    }
  });

  it('rules a table header and no body row', () => {
    expect(declares('th', 'border-bottom'), 'the header is ruled').toBe(true);
    expect(declares('td', 'border-bottom'), 'a body row is not ruled').toBe(false);
  });

  it("gives each remaining unit a mark, on a token rather than the browser's", () => {
    // The quote and the break are the sheet's marks rather than ones carried
    // over from the view this client replaced, so this pins that each has one
    // at all - unmarked, a quote reads as a paragraph and a link takes the
    // browser's own blue.
    const marks: Array<[string, string, string]> = [
      ['blockquote', 'border-left', 'a quote is marked by the rule this sheet insets with'],
      ['table', 'border-collapse: collapse', 'cells share one grid'],
      ['hr', 'background: var(--line)', 'a break is a hairline'],
      ['code', 'border-radius: 5px', "the chip takes the sheet's chip radius"],
      ['a', 'color: var(--blue)', 'a link takes a palette token'],
    ];
    for (const [unit, said, why] of marks) {
      expect(declares(unit, said), `${unit}: ${why}`).toBe(true);
    }
  });
});

describe('markdown, as the maintained module renders it', () => {
  it('renders the shapes the prose of a conversation uses', () => {
    const html = renderProse('## a heading\n\n- one\n- two\n\n`code` and **bold**');

    expect(html).toContain('<h2>');
    expect(html).toContain('<li>one</li>');
    expect(html).toContain('<code>code</code>');
    expect(html).toContain('<strong>bold</strong>');
  });

  it('renders a row own line inline: emphasis and code, no block around them', () => {
    // A row is one line by the design's rule, so its line takes the marks it
    // can draw and none of the wrappers a block would arrive in.
    const html = renderInlineProse('the **fold** joins and `cargo check` runs');

    expect(html, 'bold draws bold').toContain('<strong>fold</strong>');
    expect(html, 'code draws code').toContain('<code>cargo check</code>');
    expect(html, 'and nothing wraps it in a paragraph').not.toContain('<p>');
  });

  it('never lets raw HTML through into the page', () => {
    // A session's prose can quote anything it read - a page, a log, a file
    // from someone else's repository - and the page would run what it quotes.
    const html = renderProse('before <img src=x onerror="alert(1)"> after');

    expect(html).not.toContain('<img');
    expect(html).toContain('&lt;img');
  });

  it('draws an image as a row naming it, and makes no request', () => {
    // The module's own `image` rule draws an `<img>`, so `![](url)` in a
    // message was fetched from whatever host the text named. The row names
    // the picture and prints its source instead.
    const html = renderProse('before ![a picture](https://example.test/x.png) after');

    expect(html, 'nothing is fetched').not.toContain('<img');
    expect(html, 'the picture is named').toContain('a picture');
    expect(html, 'and its source is readable').toContain('https://example.test/x.png');
  });

  it('names an image that carries no alt by its source, once', () => {
    const html = renderProse('![](https://example.test/x.png)');

    expect(html, 'nothing is fetched').not.toContain('<img');
    expect(html.match(/https:\/\/example\.test\/x\.png/g), 'the source is the name').toHaveLength(
      1,
    );
  });

  it("draws the image row as the page's own attachment row", () => {
    // The row reuses the piece the chat draws for what a turn attached, so the
    // class and the glyph are what make it that row rather than a bare span the
    // sheet does not stylise.
    const html = renderProse('![a picture](https://example.test/x.png)');

    expect(html, 'the row takes the attachment class').toContain('class="attrow"');
    expect(html, 'and the glyph that marks it an image').toContain('href="#i-read"');
  });

  it('escapes the source it prints in the row', () => {
    // `&` is what a markdown destination gives the row raw, and the row prints
    // its source as text - so it comes out escaped rather than as the start of
    // an entity the page would resolve.
    const html = renderProse('![a picture](https://example.test/a.png?x=1&y=2)');

    expect(html, 'the source draws escaped').toContain('?x=1&amp;y=2');
  });

  it('escapes what an image names rather than drawing it as markup', () => {
    // The alt and the source are the message's own text, so neither may open
    // a tag of its own inside the row.
    const html = renderProse('![<img src=x onerror="alert(1)">](https://example.test/a.png)');

    expect(html, 'the alt cannot open a tag').not.toContain('<img');
    expect(html, 'it draws escaped').toContain('&lt;img');
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
    // The delimiters never reach the panel. What this catches is a rule that
    // slices the source range - the way the splitter in the view this client
    // replaced found a fence - rather than taking the block's body off the
    // token.
    expect(html).not.toContain('```');
  });

  it('draws no label row for a fence that carries no info string', () => {
    // The view this client replaced labelled a panel whenever the fence
    // carried one and drew no row when it did not, so a bare fence must not
    // invent a word for it.
    const html = renderProse('```\nplain text\n```');

    expect(html).toContain('<div class="code">');
    expect(html).not.toContain('class="lang"');
  });

  it('draws a fence that has not closed yet as the panel it will be', () => {
    // A message arrives a piece at a time, and a block that drew as prose until
    // its closing fence arrived would flicker on every chunk.
    expect(renderProse('```rust\nfn main() {')).toContain('<div class="lang">rust</div>');
  });

  it('draws a fence naming no language as plain text, not as a throw', () => {
    // `constructor` and `__proto__` are the two keys a lowercased lookup still
    // resolves on an object literal, and an info string is the model's own text
    // - so the one thing this must not do is throw inside the render.
    for (const info of ['constructor', '__proto__']) {
      const html = renderProse(`\`\`\`${info}\nlet a = 1 < 2;\n\`\`\``);

      expect(html, `${info}: the panel draws`).toContain('<div class="code">');
      expect(html, `${info}: the fence still labels it`).toContain(
        `<div class="lang">${info}</div>`,
      );
      expect(html, `${info}: the body is escaped text`).toContain('&lt;');
    }
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
    // A markdown fence is the same language a `.md` path names.
    expect(fenceLanguage('md')).toBe('markdown');
    expect(fenceLanguage('markdown')).toBe('markdown');
    // A fence's info string carries attributes after the language.
    expect(fenceLanguage('rust title="x"')).toBe('rust');
    expect(fenceLanguage('klingon')).toBeNull();
    expect(fenceLanguage('')).toBeNull();
  });
});

describe('a source file, as the maintained highlighter draws it', () => {
  it('takes the language from the path a call named', () => {
    expect(languageOf('crates/forge-server/src/family.rs')).toBe('rust');
    // The two most-read file types the chat left uncoloured: markdown is the
    // most-read of any type, a component tree the next uncoloured one.
    expect(languageOf('README.md')).toBe('markdown');
    expect(languageOf('docs/notes.markdown')).toBe('markdown');
    expect(languageOf('client/src/chat/Chat.svelte')).toBe('xml');
    expect(languageOf('a/file.py')).toBe('python');
    expect(languageOf('no-extension')).toBeNull();
    // A path whose extension names an object member rather than a language.
    expect(languageOf('a/x.constructor')).toBeNull();
  });

  it('marks the tokens up in the classes the highlighter emits', () => {
    const html = renderCode('rust', 'pub fn main() { let x = "one"; }');

    expect(html).toContain('hljs-');
    expect(html).toContain('pub');
  });

  it('colours a read body as markdown, not only code-shaped ones', () => {
    // A Read arrives line-numbered (`N\t` prefixes), which keeps markdown's
    // line-anchored tokens plain; the inline ones still colour, and only the
    // markdown grammar emits them for this text.
    const html = renderCode('markdown', '1\t# Title\n2\t\n3\t**bold** and `code`');

    expect(html).toContain('hljs-strong');
    expect(html).toContain('hljs-code');
  });

  it('escapes a file it has no language for rather than dropping it', () => {
    // The page never invents text a reader cannot use, and it never lets a
    // file it cannot parse through as markup either.
    const html = renderCode('rust', 'let a = 1 < 2;');

    expect(renderCode(null, 'const a = <div>;')).toContain('&lt;div&gt;');
    expect(html).toContain('&lt;');
  });
});
