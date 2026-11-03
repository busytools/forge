import { readFileSync } from 'node:fs';

import { describe, expect, it } from 'vitest';

/**
 * The document route's layout, read off the sheet.
 *
 * **Read off the declarations, not the DOM.** The route's width, its
 * scroller and its touch targets are CSS facts; a markup assertion cannot
 * see any of them, and jsdom applies no layout. The drawing beside
 * `docs/book/src/ui/client/document.md` carries the look at 1600 and 430.
 *
 * Comments are stripped first: the patterns below read everything between
 * `}` and `{` as a selector, and the document block's own comments name
 * selectors.
 */
function readable(raw: string): string {
  return raw.replace(/\/\*[\s\S]*?\*\//g, '');
}

const SHEET = readable(readFileSync(new URL('../assets/web.css', import.meta.url), 'utf8'));

/** Every rule whose selector mentions `fragment`, with its declarations. */
function rulesFor(fragment: string): { selector: string; body: string }[] {
  const escaped = fragment.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  const rules = new RegExp(`([^{}]*${escaped}[^{}]*)\\{([^{}]*)\\}`, 'g');
  return [...SHEET.matchAll(rules)].map((hit) => ({
    selector: hit[1] ?? '',
    body: hit[2] ?? '',
  }));
}

/** Every at-rule block with this header, its body read by brace matching. */
function mediaBlocks(header: string): string[] {
  const blocks: string[] = [];
  let from = 0;
  for (;;) {
    const start = SHEET.indexOf(header, from);
    if (start < 0) return blocks;
    const open = SHEET.indexOf('{', start);
    if (open < 0) return blocks;
    let depth = 0;
    for (let at = open; at < SHEET.length; at += 1) {
      if (SHEET[at] === '{') depth += 1;
      else if (SHEET[at] === '}') {
        depth -= 1;
        if (depth === 0) {
          blocks.push(SHEET.slice(open + 1, at));
          from = at + 1;
          break;
        }
      }
    }
    if (from <= open) return blocks;
  }
}

describe('the document sheet', () => {
  /** **The route fills the surface.** The body is not capped to a column,
   *  prose keeps a readable measure of its own, and code and tables take
   *  the width the window gives them. */
  it('fills the surface, with prose at a measure and code and tables at the width', () => {
    const bodies = rulesFor('.doc-page .doc-body');
    expect(
      bodies.some(
        (rule) =>
          rule.selector.trim() === '.doc-page .doc-body' && /max-width:\s*none/.test(rule.body),
      ),
      'the document body is not capped to a prose column',
    ).toBe(true);

    const prose = rulesFor('.doc-page .doc-body :is(h1, h2, h3, p, li, blockquote)');
    expect(
      prose.some((rule) => /max-width:\s*76ch/.test(rule.body)),
      'a paragraph keeps a readable measure',
    ).toBe(true);

    const wide = rulesFor('.doc-page .doc-body :is(pre, table, .code)');
    expect(
      wide.some((rule) => /max-width:\s*none/.test(rule.body)),
      'code and tables take the full width',
    ).toBe(true);
  });

  /** The body of the grouped rule that names this scroller part. */
  function skinBody(part: string): string | undefined {
    const token = `.doc-page::-webkit-scrollbar${part}`;
    return rulesFor(token).find(
      (rule) => rule.selector.includes(`${token},`) || rule.selector.trim().endsWith(token),
    )?.body;
  }

  /** **The route is its own scroller, under the one theme skin.** The page
   *  scrolls itself rather than the platform drawing a bar at the window's
   *  edge, and every part of that bar - corner included - carries the
   *  theme's declarations, so no white block survives at the bottom-right. */
  it('scrolls its own route under the theme scrollbar skin', () => {
    const page = rulesFor('.doc-page');
    expect(
      page.some(
        (rule) =>
          rule.selector.trim() === '.doc-page' &&
          /overflow-y:\s*auto/.test(rule.body) &&
          /height:\s*100dvh/.test(rule.body),
      ),
      'the document route is the scroller',
    ).toBe(true);
    expect(skinBody(''), 'the bar is the theme width').toMatch(/width:\s*8px/);
    expect(skinBody('-track'), 'the track is the ground').toMatch(/background:\s*transparent/);
    expect(skinBody('-corner'), 'the corner is the ground').toMatch(/background:\s*transparent/);
    expect(skinBody('-thumb'), 'the thumb is the theme thumb').toMatch(/background:\s*var\(--s3\)/);
  });

  /** **A finger's targets are the line numbers and the answers.** Both join
   *  the 44px floor the page's other controls carry, and that floor sits
   *  inside the coarse-pointer gate rather than anywhere on the page. */
  it('gives the gutter and the answers a 44px target under a coarse pointer', () => {
    const coarse = mediaBlocks('@media (pointer: coarse)').find((block) =>
      block.includes('.doc-page .doc-lnum'),
    );
    expect(coarse, 'the floor is gated on a coarse pointer').toBeDefined();
    expect(coarse, 'the answers share the gutter rule').toContain('.doc-page .doc-answer-btn');
    expect(coarse, 'and the rule is the 44px floor').toMatch(/min-height:\s*44px/);
  });
});
