// @vitest-environment jsdom
/**
 * The files view's markdown: the leading YAML block it hides, the rows it
 * hands the reader, and the finger targets the sheet gives them.
 *
 * **The frontmatter is blanked line for line rather than removed**, so every
 * block's `line` stays the raw file's - the number a comment anchors by.
 * A bare `---` with no closing line is a horizontal rule, not frontmatter.
 */
import { readFileSync } from 'node:fs';

import { describe, expect, it } from 'vitest';

import { markdownRows } from './markdown';

describe('the files view markdown rows', () => {
  it('hides a leading frontmatter block and keeps the raw line numbers', () => {
    const rows = markdownRows('---\ntitle: plan\nweight: 3\n---\n# Heading\n\nA paragraph.\n');

    expect(rows[0]?.line, 'the first drawn block is the raw line after the block').toBe(5);
    expect(rows[0]?.html).toContain('<h1>Heading</h1>');
    expect(rows[0]?.context, 'and its captured lines are its own').toEqual(['# Heading']);
    const drawn = rows.map((row) => row.html ?? row.text).join('\n');
    expect(drawn, 'no frontmatter text reaches the rendered output').not.toContain('title: plan');
    expect(drawn).not.toContain('weight: 3');
  });

  it('drops the hidden block when a blank line follows the closing fence', () => {
    const rows = markdownRows('---\ntitle: plan\n---\n\n# Heading\n\nA paragraph.\n');

    expect(rows[0]?.line, 'the first drawn block starts after the blank line').toBe(5);
    expect(rows[0]?.html).toContain('<h1>Heading</h1>');
    expect(
      rows.some((row) => row.html === null && row.text.trim() === ''),
      'no empty block is drawn over the hidden text',
    ).toBe(false);
  });

  it('hides frontmatter in a CRLF file', () => {
    const rows = markdownRows('---\r\ntitle: plan\r\n---\r\n# Heading\r\n');

    expect(rows[0]?.line, 'the heading keeps its raw line').toBe(4);
    expect(rows[0]?.html).toContain('<h1>Heading</h1>');
    expect(rows.map((row) => row.html ?? row.text).join('')).not.toContain('title: plan');
  });

  it('does not take a bare horizontal rule for frontmatter', () => {
    const rows = markdownRows('---\n\nbody\n');

    expect(rows[0]?.html, 'an unclosed rule renders as the rule it is').toContain('<hr>');
    expect(rows.at(-1)?.html).toContain('body');
  });

  it('draws a file that is only frontmatter as a row rather than nothing', () => {
    const rows = markdownRows('---\ntitle: only\n---\n');

    expect(rows.length, 'the reader is never handed no target at all').toBeGreaterThan(0);
    const drawn = rows.map((row) => row.html ?? row.text).join('');
    expect(drawn).not.toContain('title: only');
  });

  it('names an image and never draws an img element', () => {
    const rows = markdownRows('![the mark](https://example.com/mark.png)\n');

    expect(rows[0]?.html).toContain('the mark');
    expect(rows[0]?.html, 'the page fetches nothing a file names').not.toContain('<img');
    expect(rows[0]?.html, 'the path is printed for the reader to copy').toContain(
      'https://example.com/mark.png',
    );
  });

  it('leaves a document without frontmatter numbered from line one', () => {
    const rows = markdownRows('# hi\n\nbody\n');

    expect(rows[0]?.line).toBe(1);
    expect(rows[0]?.html).toContain('<h1>hi</h1>');
  });
});

describe('the sheet targets the rendered markdown', () => {
  /** The sheet, comments stripped, in the shape the readers below parse. */
  const SHEET = readFileSync('src/assets/web.css', 'utf8')
    .replace(/\/\*[\s\S]*?\*\//g, '')
    .replace(/\s*\{/g, ' {');

  /** The bodies of every coarse-pointer media block. */
  function coarseBodies(): string[] {
    return [
      ...SHEET.matchAll(/@media \(([^{}]*pointer: coarse[^{}]*)\) \{((?:[^{}]|\{[^{}]*\})*)\}/g),
    ].map((hit) => hit[2] ?? '');
  }

  it('gives the block and its + a 44px target under a coarse pointer', () => {
    const bodies = coarseBodies();
    expect(bodies.length, 'the sheet carries no coarse-pointer block').toBeGreaterThan(0);
    const coarse = bodies.join('\n');

    expect(coarse, 'the block itself takes the finger floor').toMatch(
      /\.files-reader\)? \.doc-block[^{]*\{[^}]*min-height:\s*44px/,
    );
    expect(coarse, 'and the line target is a 44px square').toMatch(
      /\.files-reader\)? \.doc-add[^{]*\{[^}]*width:\s*44px;[^}]*height:\s*44px/,
    );
    expect(coarse, 'drawn always: a finger has no hover').toMatch(
      /\.files-reader\)? \.doc-add[^{]*\{[^}]*opacity:\s*1/,
    );
  });

  /** **The reader must be a container, or its table guard never fires.** The
   *  stacked-table rule is a `@container (max-width: 760px)` query, and a
   *  query with no `container-type` ancestor matches nothing - the same trap
   *  the conversation wrapper and the document body are containers for. */
  it('makes the reader the container its stacked-table guard queries', () => {
    const rules = [...SHEET.matchAll(/([^{}]+)\{([^{}]*)\}/g)].map((hit) => ({
      selector: (hit[1] ?? '').trim(),
      body: hit[2] ?? '',
    }));
    const reader = rules.filter((rule) => rule.selector === '.files-markdown');
    expect(reader.length, 'the reader has a rule of its own').toBeGreaterThan(0);
    expect(
      reader.some((rule) => /container-type:\s*inline-size/.test(rule.body)),
      'a width the @container rule can resolve against',
    ).toBe(true);
    expect(
      reader.some(
        (rule) => /overflow-y:\s*auto/.test(rule.body) && /min-height:\s*0/.test(rule.body),
      ),
      'and a long file scrolls its own box rather than growing the page',
    ).toBe(true);
  });
});
