/**
 * A markdown file as the files view's comment targets: the document viewer's
 * own rows, with a leading YAML frontmatter block hidden.
 *
 * **The frontmatter is blanked line for line, never removed**: every row's
 * `line` and `end` stay the raw file's, so a comment anchors at the number
 * the file actually has. A file that opens with a bare `---` and never
 * closes it is a horizontal rule, and renders as one.
 */

import { rowsOf, type DocRow } from '../doc/comments';

/** The line the frontmatter's closing `---` sits on, or null when the file
 *  does not open with a closed YAML block. `trimEnd` so a CRLF file's
 *  `---\r` is the fence it is. */
function frontmatterEnd(lines: string[]): number | null {
  if (lines[0]?.trimEnd() !== '---') return null;
  for (let at = 1; at < lines.length; at += 1) {
    if (lines[at]?.trimEnd() === '---') return at + 1;
  }
  return null;
}

/**
 * The file's markdown rows, frontmatter hidden.
 *
 * The blanked lines still feed `rowsOf`, which is what keeps the numbering.
 * Its leading blank row - the gap the hidden block left - is dropped rather
 * than drawn as an empty target, unless it is the only row a file that is
 * nothing but frontmatter has. **The drop does not depend on how many blank
 * lines separate the closing `---` from the first block**: a blank line
 * after the fence is the common shape, and it would otherwise leave an empty
 * `.doc-block` with a `+` anchored inside text the reader cannot see.
 */
export function markdownRows(text: string): DocRow[] {
  const lines = text.split('\n');
  const end = frontmatterEnd(lines);
  const body = end === null ? text : lines.map((line, at) => (at < end ? '' : line)).join('\n');
  const rows = rowsOf(body, 'markdown');
  const first = rows[0];
  if (
    end !== null &&
    rows.length > 1 &&
    first !== undefined &&
    first.html === null &&
    first.line === 1
  ) {
    rows.shift();
  }
  return rows;
}
