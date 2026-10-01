import { readFileSync } from 'node:fs';

import { describe, expect, it } from 'vitest';

/**
 * A long line in the conversation, as the sheet draws it.
 *
 * **What this can hold and what it cannot.** A scrollbar is not in the DOM and
 * a jsdom test performs no layout, so nothing here can see the overflow this
 * file is about: the scroll was measured in a browser instead, with the diff
 * box at 822px wide against 1740px of content before the change. What a test
 * can hold is the rule that decides it - and the change worth catching is
 * someone reading "code should not wrap" into a `white-space: pre` and bringing
 * the sideways scroll back.
 *
 * The terminal is the reference here (rule 24): it wraps a long code line
 * inside its panel, wraps a diff line and re-emits the indent on the
 * continuation row, and hard-splits a token with nowhere to break
 * (`wrap_long_token` in `crates/forge-tui/src/ui/wrap.rs`).
 */
const sheet = readFileSync(new URL('../assets/web.css', import.meta.url), 'utf8');

/**
 * The bodies of every rule the sheet gives `selector`, joined.
 *
 * Every rule rather than the first: `.dif` is drawn by the conversation's block
 * and by the client's own, and a scroll could be declared in either.
 */
function bodies(selector: string): string {
  const escaped = selector.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  const rule = new RegExp(`^${escaped}\\s*\\{([^}]*)\\}`, 'gm');
  return [...sheet.matchAll(rule)].map((match) => match[1] ?? '').join('\n');
}

describe('a long line in the conversation', () => {
  it('wraps a code line inside its panel rather than scrolling it', () => {
    const body = bodies('.code pre');

    expect(body, 'the panel has a rule at all').not.toBe('');
    expect(body, 'the line wraps where the panel ends').toContain('white-space: pre-wrap');
    expect(body, 'and a token with nowhere to break ends at the edge too').toContain(
      'overflow-wrap: anywhere',
    );
    expect(body, 'so the panel is not a scroller').not.toContain('overflow-x');
  });

  it('wraps a diff line, keeping its indent, and breaks a token that cannot wrap', () => {
    const body = bodies('.dif .ln .l');

    expect(body, 'the line has a rule at all').not.toBe('');
    expect(body, 'the line wraps where the row ends').toContain('white-space: pre-wrap');
    expect(body, 'and a token with nowhere to break ends at the edge too').toContain(
      'overflow-wrap: anywhere',
    );
  });

  it('leaves the diff box itself unable to scroll sideways', () => {
    const body = bodies('.dif');

    expect(body, 'the box has a rule at all').not.toBe('');
    expect(body, 'nothing about it scrolls').not.toContain('overflow-x');
  });

  it('breaks a token in a command result that has no space to wrap at', () => {
    const body = bodies('.term');

    expect(body, 'the result box has a rule at all').not.toBe('');
    expect(body, 'a token with nowhere to break ends at the edge').toContain(
      'overflow-wrap: anywhere',
    );
    expect(body, 'so the result box is not a scroller').not.toContain('overflow-x');
  });

  it('breaks a token in a table cell so the table fits the column', () => {
    const body = bodies('.prose td');

    expect(body, 'the cell has a rule at all').not.toBe('');
    expect(body, 'a cell with one long token still fits').toContain('overflow-wrap: anywhere');
  });

  it('keeps a table header whole rather than slicing the word', () => {
    // A cell that breaks anywhere has a one-character minimum, so a
    // neighbouring column's demand for width can squeeze `Verb` into `Ve` and
    // `rb`. The header is a label, so its width is its word.
    const body = bodies('.prose th');

    expect(body, 'the header has a rule at all').not.toBe('');
    expect(body, 'the header does not break mid-word').toContain('white-space: nowrap');
  });
});
