// @vitest-environment jsdom
import { describe, expect, it } from 'vitest';

import { anchorRow, doorSpot, insertRow, lineOf, rowSide } from './diff';

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
});
