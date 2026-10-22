/**
 * A markdown change, as markdown with the change marked inline.
 *
 * The hunk's lines are rebuilt into one markdown source: a paired removal
 * and addition interleave through the word diff, a whole removed line keeps
 * its own block marker and is wrapped struck, and a whole added line is
 * wrapped as added. The source goes through the chat's ONE markdown renderer,
 * and the private-use sentinels become the strike and add spans after it -
 * so the diff draws as rendered markdown, not as a second renderer.
 */

import { wordOps, type SourceLine } from './diff';

/** Sentinels that markdown-it passes through as text, replaced after render. */
const DEL_OPEN = '\u{E000}';
const DEL_CLOSE = '\u{E001}';
const INS_OPEN = '\u{E002}';
const INS_CLOSE = '\u{E003}';

/** A block marker kept outside a whole-line wrap so the block still parses. */
const MARKER = /^(\s*(?:#{1,6}\s|>+\s|[-*+]\s|\d+[.)]\s)?)([\s\S]*)$/;

/** Why the sentinels are private-use: nothing in a real file contains them. */
const SENTINELS = new Map([
  [DEL_OPEN, '<span class="mddel">'],
  [DEL_CLOSE, '</span>'],
  [INS_OPEN, '<span class="mdadd">'],
  [INS_CLOSE, '</span>'],
]);

/** One paired line, both sides inline in reading order. */
function merged(oldText: string, newText: string): string {
  return wordOps(oldText, newText)
    .map((op) =>
      op.kind === 'equal'
        ? op.text
        : op.kind === 'del'
          ? `${DEL_OPEN}${op.text}${DEL_CLOSE}`
          : `${INS_OPEN}${op.text}${INS_CLOSE}`,
    )
    .join('');
}

/** A whole line wrapped, its block marker left where the parser needs it. */
function whole(text: string, kind: 'del' | 'add'): string {
  const match = MARKER.exec(text);
  const marker = match?.[1] ?? '';
  const rest = match?.[2] ?? text;
  if (rest === '') return text;
  return kind === 'del'
    ? `${marker}${DEL_OPEN}${rest}${DEL_CLOSE}`
    : `${marker}${INS_OPEN}${rest}${INS_CLOSE}`;
}

/** A hunk's lines and a call's own two sides, as one list of source lines. */
export function patchLines(
  pieces: readonly (
    | { kind: 'diff'; old: string; new: string }
    | { kind: 'hunk'; header: string; lines: SourceLine[] }
  )[],
): SourceLine[] {
  const lines: SourceLine[] = [];
  for (const piece of pieces) {
    if (piece.kind === 'hunk') {
      lines.push(...piece.lines.map(({ kind, text }) => ({ kind, text })));
      continue;
    }
    if (piece.old !== '') {
      lines.push(...piece.old.split('\n').map((text) => ({ kind: 'del' as const, text })));
    }
    if (piece.new !== '') {
      lines.push(...piece.new.split('\n').map((text) => ({ kind: 'add' as const, text })));
    }
  }
  return lines;
}

/** The change as markdown source, the change marked with sentinels. */
export function markdownDiffSource(lines: readonly SourceLine[]): string {
  const out: string[] = [];
  let i = 0;
  while (i < lines.length) {
    if (lines[i]?.kind !== 'del') {
      const line = lines[i];
      out.push(line === undefined ? '' : line.kind === 'add' ? whole(line.text, 'add') : line.text);
      i += 1;
      continue;
    }
    let removed = i;
    while (removed < lines.length && lines[removed]?.kind === 'del') removed += 1;
    let added = removed;
    while (added < lines.length && lines[added]?.kind === 'add') added += 1;
    const pairs = Math.min(removed - i, added - removed);
    for (let k = 0; k < pairs; k += 1) {
      const oldLine = lines[i + k];
      const newLine = lines[removed + k];
      if (oldLine === undefined || newLine === undefined) continue;
      out.push(merged(oldLine.text, newLine.text));
    }
    for (let k = i + pairs; k < removed; k += 1) {
      const line = lines[k];
      if (line !== undefined) out.push(whole(line.text, 'del'));
    }
    for (let k = removed + pairs; k < added; k += 1) {
      const line = lines[k];
      if (line !== undefined) out.push(whole(line.text, 'add'));
    }
    i = added;
  }
  return out.join('\n');
}

/** The rendered hunk, with the sentinels standing as the strike and add spans. */
export function markdownDiffHtml(
  lines: readonly SourceLine[],
  render: (markdown: string) => string,
): string {
  let html = render(markdownDiffSource(lines));
  for (const [sentinel, tag] of SENTINELS) html = html.replaceAll(sentinel, tag);
  return html;
}
