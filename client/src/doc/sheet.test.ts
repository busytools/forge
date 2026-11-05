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
  /** **The sheet is a wide centred page.** The page background stays full
   *  width, the body uses the space at a wide cap and is centred, and prose
   *  runs the sheet's whole width rather than a narrow measure. */
  it('centres a wide sheet, with prose filling it', () => {
    const page = rulesFor('.doc-page');
    expect(
      page.some(
        (rule) => rule.selector.trim() === '.doc-page' && /max-width:\s*none/.test(rule.body),
      ),
      'the page background stays full width',
    ).toBe(true);

    const bodies = rulesFor('.doc-page .doc-body');
    expect(
      bodies.some(
        (rule) =>
          rule.selector.trim() === '.doc-page .doc-body' &&
          /max-width:\s*1400px/.test(rule.body) &&
          /margin-inline:\s*auto/.test(rule.body),
      ),
      'the document body is a wide centred sheet',
    ).toBe(true);

    const prose = rulesFor('.doc-page .doc-body :is(h1, h2, h3, p, li, blockquote)');
    expect(
      prose.some((rule) => /max-width:/.test(rule.body)),
      'prose runs the sheet, not a narrow measure',
    ).toBe(false);
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

  /** **A finger's targets are the hover `+` and the answers.** Both join
   *  the 44px floor the page's other controls carry, inside the
   *  coarse-pointer gate, and the `+` is drawn always there because a finger
   *  has no hover to reveal it. */
  it('gives the hover target and the answers a 44px target under a coarse pointer', () => {
    const coarse = mediaBlocks('@media (pointer: coarse)').find((block) =>
      block.includes('.doc-page .doc-add'),
    );
    expect(coarse, 'the floor is gated on a coarse pointer').toBeDefined();
    expect(coarse, 'the answers share the hover target rule').toContain(
      '.doc-page .doc-answer-btn',
    );
    expect(coarse, 'and the rule is the 44px floor').toMatch(/min-height:\s*44px/);
    expect(coarse, 'the `+` is always drawn for a finger').toMatch(/opacity:\s*1/);
    expect(coarse, 'the `+` itself takes the 44px floor, not any neighbour').toMatch(
      /\.doc-page \.doc-add\s*\{[^}]*width:\s*44px[^}]*height:\s*44px/,
    );
  });

  /** **The `+` is a plain margin mark, revealed by hover or focus.** No chip,
   *  no rule; the block under the pointer shows it, focus-within covers the
   *  keyboard, and the base rule keeps it out of sight otherwise. */
  it('reveals a plain + on hover or focus', () => {
    const base = rulesFor('.doc-add').find((rule) => rule.selector.trim() === '.doc-page .doc-add');
    expect(base, 'the base rule exists').toBeDefined();
    expect(base?.body, 'the `+` is hidden until the block is under the pointer').toMatch(
      /opacity:\s*0/,
    );
    expect(base?.body, 'the `+` is a mark, not a chip').toMatch(/border:\s*0/);
    expect(base?.body, 'with no ground of its own').toMatch(/background:\s*none/);
    expect(base?.body, 'and no circle').not.toMatch(/border-radius/);
    const reveal = rulesFor('.doc-add').find(
      (rule) =>
        /\.doc-block:hover\s+\.doc-add/.test(rule.selector) &&
        /\.doc-block:focus-within\s+\.doc-add/.test(rule.selector),
    );
    expect(reveal, 'hover and focus share the reveal rule').toBeDefined();
    expect(reveal?.body, 'and the reveal is drawn').toMatch(/opacity:\s*1/);

    const hover = rulesFor('.doc-block:hover').find((rule) =>
      rule.selector.includes('.doc-block:hover'),
    );
    expect(hover, 'the block highlight rule exists').toBeDefined();
    expect(hover?.body, 'the highlight is neutral, with no accent bar').not.toMatch(
      /--accent|box-shadow/,
    );

    const block = rulesFor('.doc-page .doc-block').find(
      (rule) => rule.selector.trim() === '.doc-page .doc-block',
    );
    expect(block?.body, 'the highlight band gives both sides air').toMatch(/padding:\s*0 12px/);
    expect(block?.body, 'and is the same width on both sides').toMatch(/margin:\s*0 -12px/);
  });

  /** **A table's first column keeps room to read as a name.** Auto layout
   *  otherwise ribbons it while the prose columns take the width. */
  it("gives a table's first column a name-width floor", () => {
    const cells = rulesFor('.doc-page .doc-block :is(th, td):first-child').find((rule) =>
      rule.selector.includes(':first-child'),
    );
    expect(cells, 'the first-column rule exists').toBeDefined();
    expect(cells?.body, 'a minimum width').toMatch(/min-width:\s*11rem/);
    expect(cells?.body, 'and a share of the table').toMatch(/width:\s*20%/);
  });

  /** **The answer row sits in the same column as the sheet.** Its width and
   *  padding follow the sheet's, the phone inset follows the sheet's own
   *  padding, and a refusal draws where the buttons are. */
  it('aligns the answer row to the reading column', () => {
    const exit = rulesFor('.doc-exit').find(
      (rule) => rule.selector.trim() === '.doc-page .doc-exit',
    );
    expect(exit, 'the exit rule exists').toBeDefined();
    expect(exit?.body, 'the exit keeps the sheet width').toMatch(/max-width:\s*1400px/);
    expect(exit?.body, 'and is centred with the sheet').toMatch(/margin-inline:\s*auto/);
    expect(exit?.body, "and carries the sheet's inset").toMatch(/padding:\s*14px 48px 0/);

    const phone = mediaBlocks('@media (max-width: 760px)').find((block) =>
      block.includes('.doc-page .doc-exit'),
    );
    expect(phone, 'the phone inset follows the sheet').toMatch(/padding-inline:\s*20px/);

    const refused = rulesFor('.doc-exit .doc-said').find((rule) =>
      rule.selector.includes('.doc-exit .doc-said'),
    );
    expect(refused?.body, 'a refused answer is drawn at the buttons').toMatch(/margin-top:\s*0/);
  });
});
