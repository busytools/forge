// @vitest-environment jsdom
import { describe, expect, it } from 'vitest';

import type { DiffFileRecord } from '../session/wire';
import {
  anchorRow,
  commentAnchor,
  doorSpot,
  gapsOf,
  insertRow,
  lineOf,
  revealedLines,
  rowSide,
  type FileLines,
} from './diff';

/**
 * The canvas's DOM decisions over the viewer's own shapes.
 *
 * **The tables below are captures of a live page, not guesses**: unified
 * rows carry one number cell holding both numbers (a context line has two),
 * split rows carry a cell per side, and a split row pads its missing half
 * with one spanning placeholder cell. Mounting the viewer itself under
 * jsdom is not possible - its text measurement allocates without bound
 * there - so the shapes are built by hand and the page's own functions run
 * over them.
 */

/** A split row for a context line: both sides real, one cell each. */
function splitContextRow(oldLine: number, newLine: number): HTMLTableRowElement {
  const row = document.createElement('tr');
  row.className = 'diff-line';
  row.innerHTML = `
    <td class="diff-line-old-num" data-side="old"><span data-line-num="${oldLine}">${oldLine}</span></td>
    <td class="diff-line-old-content" data-side="old">code</td>
    <td class="diff-line-new-num" data-side="new"><span data-line-num="${newLine}">${newLine}</span></td>
    <td class="diff-line-new-content" data-side="new">code</td>`;
  return row;
}

/** A split row for an added line: the old half is one spanning placeholder. */
function splitAddedRow(newLine: number): HTMLTableRowElement {
  const row = document.createElement('tr');
  row.className = 'diff-line';
  row.innerHTML = `
    <td class="diff-line-old-placeholder select-none" colspan="2"></td>
    <td class="diff-line-new-num" data-side="new"><span data-line-num="${newLine}">${newLine}</span></td>
    <td class="diff-line-new-content" data-side="new">code</td>`;
  return row;
}

/** A split row for a removed line: the new half is the placeholder. */
function splitRemovedRow(oldLine: number): HTMLTableRowElement {
  const row = document.createElement('tr');
  row.className = 'diff-line';
  row.innerHTML = `
    <td class="diff-line-old-num" data-side="old"><span data-line-num="${oldLine}">${oldLine}</span></td>
    <td class="diff-line-old-content" data-side="old">code</td>
    <td class="diff-line-new-placeholder select-none" colspan="2"></td>`;
  return row;
}

/** A unified row: one number cell carrying whichever sides the line has. */
function unifiedRow(oldLine: number | null, newLine: number | null): HTMLTableRowElement {
  const row = document.createElement('tr');
  row.className = 'diff-line';
  const old = oldLine === null ? '' : `<span data-line-old-num="${oldLine}">${oldLine}</span>`;
  const fresh = newLine === null ? '' : `<span data-line-new-num="${newLine}">${newLine}</span>`;
  row.innerHTML = `
    <td class="diff-line-num">${old}${fresh}</td>
    <td class="diff-line-content">code</td>`;
  return row;
}

function body(html: string): HTMLElement {
  const el = document.createElement('div');
  el.innerHTML = html;
  return el;
}

/**
 * A split table as the viewer wraps it: four columns, the two sides named by
 * the colgroup, which is where a split insert reads its halves from.
 */
function splitTable(...rows: HTMLTableRowElement[]): HTMLTableElement {
  const table = document.createElement('table');
  table.innerHTML = `<colgroup>
    <col class="diff-table-old-num-col" width="40" />
    <col class="diff-table-old-content-col" />
    <col class="diff-table-new-num-col" width="40" />
    <col class="diff-table-new-content-col" />
  </colgroup><tbody></tbody>`;
  table.querySelector('tbody')?.append(...rows);
  return table;
}

/** The card a test hands an insert, so a row can be asked where it landed. */
function card(): HTMLElement {
  const el = document.createElement('div');
  el.textContent = 'card';
  return el;
}

/** One file whose two hunks leave eight lines between them, for the gap tests. */
const GAPPED: DiffFileRecord = {
  path: 'src/a.rs',
  oldPath: null,
  status: 'modified',
  binary: false,
  submodule: false,
  truncated: false,
  hunks: [
    {
      oldStart: 5,
      oldCount: 1,
      newStart: 5,
      newCount: 1,
      lines: [{ kind: 'context', text: 'five', oldLine: 5, newLine: 5 }],
    },
    {
      oldStart: 14,
      oldCount: 1,
      newStart: 14,
      newCount: 1,
      lines: [{ kind: 'context', text: 'fourteen', oldLine: 14, newLine: 14 }],
    },
  ],
};

/** That file's whole content, as the read answers it. */
const GAPPED_LINES: FileLines = {
  lines: Array.from({ length: 20 }, (_, i) => `l${i + 1}`),
  truncated: false,
};

describe('the viewer shapes the page reads back', () => {
  it('names the line a number span carries, whichever attribute form it uses', () => {
    // The split shape carries both the attribute and the text; the unified
    // shape carries the side in the attribute and the number in the text.
    const split = document.createElement('span');
    split.dataset['lineNum'] = '12';
    split.textContent = '12';
    expect(lineOf(split)).toBe(12);
    const unified = document.createElement('span');
    unified.dataset['lineNewNum'] = '9';
    unified.textContent = '9';
    expect(lineOf(unified), 'the unified side attribute names it').toBe(9);
    const empty = document.createElement('span');
    expect(lineOf(empty), 'a cell with no number names no line').toBeNull();
  });

  it('reads a split click as the side it landed on, and a unified one as new-first', () => {
    const context = splitContextRow(6, 9);
    const newContent = context.children[3] as HTMLElement;
    const oldNum = context.children[0] as HTMLElement;
    expect(rowSide(context, newContent, 'split'), 'the right half is the new side').toEqual({
      side: 'new',
      line: 9,
    });
    expect(rowSide(context, oldNum, 'split'), 'the left half is the old side').toEqual({
      side: 'old',
      line: 6,
    });
    expect(rowSide(splitRemovedRow(4), null, 'split'), 'a removed line is old').toEqual({
      side: 'old',
      line: 4,
    });
    expect(rowSide(unifiedRow(6, 9), null, 'unified'), 'a unified context line is new').toEqual({
      side: 'new',
      line: 9,
    });
    expect(
      rowSide(unifiedRow(4, null), null, 'unified'),
      'a unified removed line has only the old',
    ).toEqual({ side: 'old', line: 4 });
  });

  it('finds the row a side and line sit on, in both modes', () => {
    const split = body('<table><tbody></tbody></table>');
    const splitRow = splitContextRow(6, 9);
    split.querySelector('tbody')?.append(splitRow);
    expect(anchorRow(split, 'new', 9, 'split')).toBe(splitRow);
    expect(anchorRow(split, 'old', 6, 'split')).toBe(splitRow);
    expect(anchorRow(split, 'new', 7, 'split'), 'a line the diff does not carry').toBeNull();

    const unified = body('<table><tbody></tbody></table>');
    const unifiedLine = unifiedRow(6, 9);
    unified.querySelector('tbody')?.append(unifiedLine);
    expect(anchorRow(unified, 'new', 9, 'unified')).toBe(unifiedLine);
    expect(anchorRow(unified, 'old', 6, 'unified')).toBe(unifiedLine);
  });

  it('places the door on its own side in split, and in the number cell in unified', () => {
    const context = splitContextRow(6, 9);
    expect(doorSpot(context, 'new', 'split')?.cell, 'the new door owns the right number cell').toBe(
      context.children[2],
    );
    expect(doorSpot(context, 'new', 'split')?.line).toBe(9);
    expect(doorSpot(context, 'old', 'split')?.cell, 'the old door owns the left one').toBe(
      context.children[0],
    );
    expect(
      doorSpot(splitAddedRow(9), 'old', 'split'),
      'a half the row does not carry has no door',
    ).toBeNull();
    expect(doorSpot(context, 'new', 'unified'), 'unified has no per-side cell').toBeNull();
  });

  it('shapes an inserted row after its anchor: the half in split, the lane in unified', () => {
    const card = (): HTMLElement => {
      const el = document.createElement('div');
      el.textContent = 'card';
      return el;
    };
    // Each half is two of the table's four columns, whichever cells the
    // row's own side is made of.
    const fromContext = insertRow(splitContextRow(6, 9), 'new', 'split', card());
    expect([...fromContext.children].map((c) => c.getAttribute('colspan'))).toEqual(['2', '2']);
    expect(fromContext.children[0]?.textContent, 'the left half stays empty').toBe('');
    expect(fromContext.children[1]?.textContent).toBe('card');

    const oldSide = insertRow(splitContextRow(6, 9), 'old', 'split', card());
    expect(oldSide.children[0]?.textContent, 'an old-side card holds the left half').toBe('card');
    expect(oldSide.children[1]?.textContent).toBe('');

    const fromAdded = insertRow(splitAddedRow(9), 'new', 'split', card());
    expect(
      [...fromAdded.children].map((c) => c.getAttribute('colspan')),
      'the placeholder half spans what the placeholder spans',
    ).toEqual(['2', '2']);
    expect(fromAdded.children[1]?.textContent).toBe('card');

    const unifiedLine = insertRow(unifiedRow(6, 9), 'new', 'unified', card());
    expect(unifiedLine.children.length, 'unified takes the whole lane in one cell').toBe(1);
    expect(unifiedLine.children[0]?.getAttribute('colspan')).toBe('2');
    expect(unifiedLine.children[0]?.textContent).toBe('card');
  });

  /**
   * **An insert never claims more columns than its anchor row has.** A card
   * that spanned the whole row would leave its own half, and on a
   * content-sized table it would rebalance every column under it. The three
   * row shapes a card can land under each keep their own cell count.
   */
  it('covers exactly its anchor row columns, card inside one half', () => {
    const spans = (row: Element): number =>
      [...row.children].reduce((n, cell) => n + Number(cell.getAttribute('colspan') ?? 1), 0);
    const shapes = [
      ['a context line', splitContextRow(6, 9)],
      ['a removed line', splitRemovedRow(4)],
      ['an added line', splitAddedRow(9)],
    ] as const;
    for (const [shape, anchor] of shapes) {
      splitTable(anchor);
      for (const side of ['old', 'new'] as const) {
        const row = insertRow(anchor, side, 'split', card());
        expect(spans(row), `${side} insert under ${shape} spans the same columns`).toBe(
          spans(anchor),
        );
        expect(
          [...row.children].filter((cell) => cell.textContent === 'card').length,
          `${side} insert under ${shape} holds the card in exactly one cell`,
        ).toBe(1);
      }
    }
  });

  /**
   * **A reachable row keeps its own side's half, and the table's columns.**
   * The shapes a card is really anchored to are the ones `anchorRow` finds -
   * a sided number span: a context line, a removed line whose new half is
   * the spanning placeholder, an added line whose old half is. The insert
   * covers exactly the table's four columns, the card sits in its side, and
   * the columns the viewer declares do not change under it. (The widths
   * themselves were measured on the live page; jsdom lays nothing out, so
   * this pins the structure those widths follow.)
   */
  it("keeps a reachable row's halves, and the table's columns, under an insert", () => {
    const cases = [
      ['a removed line', splitRemovedRow(4), 'old', 0],
      ['a context line', splitContextRow(6, 9), 'old', 0],
      ['a context line', splitContextRow(6, 9), 'new', 1],
      ['an added line', splitAddedRow(9), 'new', 1],
    ] as const;
    for (const [shape, anchor, side, at] of cases) {
      const table = splitTable(anchor);
      const before = [...table.querySelectorAll(':scope > colgroup > col')].map(
        (col) => col.className,
      );
      const row = insertRow(anchor, side, 'split', card());
      expect(
        [...row.children].map((cell) => cell.getAttribute('colspan')),
        `${side} insert under ${shape} keeps both halves`,
      ).toEqual(['2', '2']);
      expect(row.children[at]?.textContent, `${side} insert under ${shape} holds the card`).toBe(
        'card',
      );
      expect(
        [...table.querySelectorAll(':scope > colgroup > col')].map((col) => col.className),
        `${side} insert under ${shape} leaves the columns as they were`,
      ).toEqual(before);
    }
  });

  /**
   * **The side-less row is a guard, not a reachable anchor.** `anchorRow`
   * finds a row only by a sided number span, so a row this shape cannot be
   * what a card lands under; the table's own columns are what keep the
   * insert honest if one ever reaches `insertRow`. The reader's context
   * expansion does not produce these rows on this page - the viewer's extend
   * widget is off for a text-built diff - so this pins the guard only.
   */
  it('takes its halves from the table when the anchor row names no side', () => {
    const table = splitTable(splitContextRow(6, 9));
    /** A row with one cell across the table: its cells name no side at all. */
    const naked = document.createElement('tr');
    naked.className = 'diff-line diff-line-extend';
    naked.innerHTML = '<td colspan="4">expanded context</td>';
    table.querySelector('tbody')?.append(naked);

    for (const anchor of [naked]) {
      const left = insertRow(anchor, 'old', 'split', card());
      expect(
        [...left.children].map((cell) => cell.getAttribute('colspan')),
        'a left insert keeps the left half',
      ).toEqual(['2', '2']);
      expect(left.children[0]?.textContent, 'and the card is on the left').toBe('card');
      const right = insertRow(anchor, 'new', 'split', card());
      expect(
        [...right.children].map((cell) => cell.getAttribute('colspan')),
        'a right insert keeps the right half',
      ).toEqual(['2', '2']);
      expect(right.children[1]?.textContent, 'and the card is on the right').toBe('card');
    }
  });
});

describe('the lines a comment can anchor to', () => {
  /**
   * **A revealed line takes a comment like any other** (Ved, 2026-10-22):
   * the expansion's lines come from the file read, not the record's hunks,
   * so the anchor's text and neighbours come from the content. A line the
   * reader has not revealed, or one the read did not carry, anchors nothing.
   */
  it('anchors a revealed gap line to the content it was drawn from', () => {
    const gaps = gapsOf(GAPPED, GAPPED_LINES);
    const between = gaps.find((gap) => gap.after === 0);
    if (between === undefined) throw new Error('the between-hunks gap is missing');
    const revealed = revealedLines([between], new Map([[0, { down: 2, up: 0 }]]));

    expect(
      commentAnchor(GAPPED, GAPPED_LINES, revealed, splitContextRow(6, 6), 'new', 6, 'split'),
      'the first revealed line on the new side',
    ).toEqual({ line: 6, contentIndex: 5 });
    expect(
      commentAnchor(GAPPED, GAPPED_LINES, revealed, splitContextRow(6, 6), 'old', 6, 'split'),
      "and on the old side, through the row's paired new number",
    ).toEqual({ line: 6, contentIndex: 5 });
    expect(
      commentAnchor(GAPPED, GAPPED_LINES, revealed, unifiedRow(6, 6), 'new', 6, 'unified'),
      'and in unified, whose one cell carries the new number',
    ).toEqual({ line: 6, contentIndex: 5 });

    expect(
      commentAnchor(GAPPED, GAPPED_LINES, revealed, splitContextRow(8, 8), 'new', 8, 'split'),
      'a line past the two revealed takes none',
    ).toBeNull();
    expect(
      commentAnchor(GAPPED, GAPPED_LINES, new Set(), splitContextRow(6, 6), 'new', 6, 'split'),
      'and neither does a line with no reveal at all',
    ).toBeNull();

    const fromBottom = revealedLines([between], new Map([[0, { down: 0, up: 2 }]]));
    expect(
      commentAnchor(GAPPED, GAPPED_LINES, fromBottom, splitContextRow(13, 13), 'new', 13, 'split'),
      "an up press reveals from the gap's other end",
    ).toEqual({ line: 13, contentIndex: 12 });

    expect(
      commentAnchor(GAPPED, GAPPED_LINES, revealed, splitContextRow(5, 5), 'new', 5, 'split'),
      "a hunk's own line anchors from the record, with no content index",
    ).toEqual({ line: 5, contentIndex: null });
    expect(
      commentAnchor(GAPPED, null, revealed, splitContextRow(6, 6), 'new', 6, 'split'),
      'a read that carried no content anchors nothing',
    ).toBeNull();
  });
});
