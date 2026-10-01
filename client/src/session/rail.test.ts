import { readFileSync } from 'node:fs';

import { render } from 'svelte/server';
import { describe, expect, it } from 'vitest';

import { homeWire } from '../dev/fixture.data';
import type { SessionSlot } from '../wire/types';
import Rail from './Rail.svelte';

/**
 * The rail's own surface, and the rules it draws by.
 *
 * **Where each check can look, and why.** A gap between two figures in a row is
 * a question about TEXT, so it is asserted against the rendered row, which is
 * the artifact the defect is in. Which way a summary aligns its items is a
 * DECLARATION, and jsdom performs no layout, so no rendered assertion can see
 * it: that one is read off the sheet by name. Neither claims anything about
 * what a browser paints - the by-width measurement in the pull request does.
 */

const sheet = readFileSync(new URL('../assets/web.css', import.meta.url), 'utf8');

const LEAD: SessionSlot = { org: 'TestOrg', project: 'proj', label: 'lead' };

const footer = render(Rail, {
  props: { home: homeWire, current: LEAD, now: Date.now(), onclose: () => undefined },
}).body;

/** One rule's declarations, from its selector to the brace that closes it. */
function rule(selector: string): string {
  const at = sheet.indexOf(`${selector} {`);
  expect(at, `${selector} is in the sheet`).toBeGreaterThan(-1);
  const end = sheet.indexOf('}', at);
  return sheet.slice(at, end);
}

describe('the rail footer', () => {
  it('separates the installed version from the one it moves to', () => {
    // The fixture answers 1.0.0 installed against 1.1.0 published, so the row
    // draws both halves and the separator between them is the whole subject.
    // Read as TEXT rather than as markup: the arrow and the version are two
    // nodes with nothing between them, which is what a reader saw run together.
    const row = footer.slice(footer.indexOf('claude v'));
    const words = row
      .slice(0, row.indexOf('</div>'))
      .replace(/<!--.*?-->/g, '')
      .replace(/<[^>]*>/g, '');
    expect(words, 'the row reads as two figures, not as one').toContain(
      'claude v1.0.0 \u{2192} v1.1.0',
    );
  });

  /**
   * **The row is a transition, and the notice is the half that carries the
   * colour.** The arrow opens the notice span rather than standing between the
   * two versions as a mark of its own: a side arrow is what says the pair is
   * one version moving to another, where the up arrow it replaced read as a
   * badge on the second one. The installed half stays `--muted`; the colour is
   * what says which half is the notice.
   *
   * Read as markup, because whether the arrow sits inside the coloured span is
   * a question about the tree, and no text assertion can see it.
   */
  it('opens the notice with the side arrow, and colours that half alone', () => {
    expect(footer, 'the arrow is no longer part of the notice').toContain(
      '<span class="up">\u{2192} v1.1.0</span>',
    );
    expect(rule('.rfoot .vers .up'), 'the notice lost its colour').toContain('var(--warn)');
    expect(rule('.rfoot .vers .v'), 'the installed half took the notice colour').not.toContain(
      'var(--warn)',
    );
  });
});

describe('the asleep heading', () => {
  it('centres its chevron on the row rather than on the text baseline', () => {
    // `baseline` rides the icon on the label's baseline, which drew it about
    // 4px above the row's centre - the one fold on the page that did.
    const gfold = rule('details.gfold > summary');
    expect(gfold).toContain('align-items: center');
    expect(gfold, 'and not the baseline it was drawn on').not.toContain('align-items: baseline');
  });
});
