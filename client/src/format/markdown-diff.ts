/**
 * A markdown change, as markdown with the change marked inline.
 *
 * **Block markers are decided before lines are paired.** A heading, a fence
 * and a table row carry their own structure, so they are drawn as whole
 * blocks rather than word-merged: a paragraph promoted to a heading must
 * still render as a heading, and a removed code fence must stay a code block
 * with its body struck. Fence bodies are wrapped inside the fence, where the
 * sentinels are literal text until the post-render pass; the markers
 * themselves are never wrapped, or the block would stop parsing.
 *
 * The source goes through the chat's ONE markdown renderer, and the
 * private-use sentinels become the strike and add spans after it. A change
 * inside a link DESTINATION falls back to whole lines, because a destination
 * cannot draw both sides; link attributes are cleared of sentinels before
 * the span pass, so no encoded sentinel can reach an href.
 */

import { wordOps, wordSpans, type SourceLine, type Span } from './diff';

/** Sentinels that markdown-it passes through as text, replaced after render. */
const DEL_OPEN = '\u{E000}';
const DEL_CLOSE = '\u{E001}';
const INS_OPEN = '\u{E002}';
const INS_CLOSE = '\u{E003}';

const OPEN = { del: DEL_OPEN, add: INS_OPEN } as const;
const CLOSE = { del: DEL_CLOSE, add: INS_CLOSE } as const;

/** A block marker kept outside a whole-line wrap so the block still parses. */
const MARKER = /^(\s*(?:#{1,6}\s|>+\s|[-*+]\s|\d+[.)]\s)?)([\s\S]*)$/;

/** A fenced block's own delimiter, at any indentation. */
const FENCE = /^\s*(```|~~~)/;

/** A pipe at the start of a table row, which the parser needs outside a wrap. */
const TABLE = /^\s*\|/;

/** Why the sentinels are private-use: nothing in a real file contains them. */
const SENTINELS = new Map([
  [DEL_OPEN, '<span class="mddel">'],
  [DEL_CLOSE, '</span>'],
  [INS_OPEN, '<span class="mdadd">'],
  [INS_CLOSE, '</span>'],
]);

/** Every way a renderer can spell a sentinel in an attribute value. */
const ENCODED = [...SENTINELS.keys()].flatMap((sentinel) => [
  sentinel,
  encodeURIComponent(sentinel),
  encodeURIComponent(sentinel).toLowerCase(),
]);

function isFence(text: string): boolean {
  return FENCE.test(text);
}

function isTable(text: string): boolean {
  return TABLE.test(text);
}

function isHeading(text: string): boolean {
  return /^\s*#{1,6}\s/.test(text);
}

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

/** A whole line's content wrapped, no block marker parsing. */
function wrapped(text: string, kind: 'del' | 'add'): string {
  return text === '' ? text : `${OPEN[kind]}${text}${CLOSE[kind]}`;
}

/** A whole line wrapped, its block marker left where the parser needs it. */
function whole(text: string, kind: 'del' | 'add'): string {
  const match = MARKER.exec(text);
  const marker = match?.[1] ?? '';
  const rest = match?.[2] ?? text;
  if (rest === '') return text;
  // A table row's pipes are structure, and a sentinel may not cross a cell:
  // each cell is wrapped on its own, inside its own pipes.
  if (isTable(text)) return tableRow(text, kind);
  return `${marker}${OPEN[kind]}${rest}${CLOSE[kind]}`;
}

/** A table row with each cell wrapped separately, its pipes kept outside. */
function tableRow(text: string, kind: 'del' | 'add'): string {
  const cells = text.split('|');
  return cells
    .map((cell, at) =>
      at === 0 || at === cells.length - 1 || cell.trim() === ''
        ? cell
        : `${OPEN[kind]}${cell}${CLOSE[kind]}`,
    )
    .join('|');
}

/** The character span of each link destination in `text`. */
function destinations(text: string): Span[] {
  const out: Span[] = [];
  for (const match of text.matchAll(/\]\(([^)]*)\)/g)) {
    const at = (match.index ?? 0) + 2;
    out.push({ start: at, end: at + (match[1]?.length ?? 0) });
  }
  return out;
}

/** Whether a change touches a link destination, which cannot draw both sides. */
function touchesLink(text: string, spans: readonly Span[]): boolean {
  return destinations(text).some((destination) =>
    spans.some((span) => span.start < destination.end && span.end > destination.start),
  );
}

/** Whether two lines can be word-merged: plain lines, away from link targets. */
function mergeable(oldText: string, newText: string): boolean {
  if (isFence(oldText) || isFence(newText)) return false;
  if (isHeading(oldText) || isHeading(newText)) return false;
  if (isTable(oldText) || isTable(newText)) return false;
  const spans = wordSpans(oldText, newText);
  return !touchesLink(oldText, spans.old) && !touchesLink(newText, spans.new);
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
  let insideFence = false;
  let i = 0;
  while (i < lines.length) {
    const line = lines[i];
    if (line === undefined) break;

    if (isFence(line.text)) {
      // **Every** fence marker is literal and toggles the block, wherever it
      // sits in a run: a marker is a boundary, never wrapped and never
      // word-paired. A whole-fence replacement is therefore a close and an
      // open, which draws the old block and the new one separately.
      out.push(line.text);
      insideFence = !insideFence;
      i += 1;
      continue;
    }

    if (insideFence) {
      // Inside a fence the sentinels are code text until the render pass;
      // the body keeps its block, struck when the line is removed.
      out.push(line.kind === 'del' ? wrapped(line.text, 'del') : line.text);
      i += 1;
      continue;
    }

    if (line.kind !== 'del') {
      out.push(line.kind === 'add' ? whole(line.text, 'add') : line.text);
      i += 1;
      continue;
    }

    // A run of removed lines and the added lines right after it, both
    // BOUNDED BY a fence marker: the marker is handled as a block, so a
    // fence later in a run still opens its block instead of being wrapped.
    let removed = i;
    while (
      removed < lines.length &&
      lines[removed]?.kind === 'del' &&
      !isFence(lines[removed]?.text ?? '')
    ) {
      removed += 1;
    }
    let added = removed;
    while (
      added < lines.length &&
      lines[added]?.kind === 'add' &&
      !isFence(lines[added]?.text ?? '')
    ) {
      added += 1;
    }
    const pairs = Math.min(removed - i, added - removed);
    for (let k = 0; k < pairs; k += 1) {
      const oldLine = lines[i + k];
      const newLine = lines[removed + k];
      if (oldLine === undefined || newLine === undefined) continue;
      if (mergeable(oldLine.text, newLine.text)) {
        out.push(merged(oldLine.text, newLine.text));
      } else {
        out.push(whole(oldLine.text, 'del'));
        out.push(whole(newLine.text, 'add'));
      }
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

/** `value` with every spelling of a sentinel removed. */
export function plain(value: string): string {
  let out = value;
  for (const encoded of ENCODED) out = out.replaceAll(encoded, '');
  return out;
}

/** A reference-style link definition: `[name]: https://...`. */
const DEFINITION = /^\s*\[[^\]]+\]:\s*\S/;

/**
 * Whether a hunk changes a reference-style link definition.
 *
 * **A rendered diff cannot draw one honestly**: a definition produces no
 * output, and a sentinel inside it makes both the definition and every
 * reference draw literally. Such a hunk falls back to the source diff.
 */
export function hasDefinitionChange(lines: readonly SourceLine[]): boolean {
  return lines.some((line) => line.kind !== 'ctx' && DEFINITION.test(line.text));
}

/** The rendered hunk, with the sentinels standing as the strike and add spans. */
export function markdownDiffHtml(
  lines: readonly SourceLine[],
  render: (markdown: string) => string,
): string {
  let html = render(markdownDiffSource(lines));
  // A link's destination cannot draw both sides and must never carry a
  // sentinel: clear the attributes before the span pass.
  html = html.replace(
    /\b(href|src|title)="([^"]*)"/g,
    (_, name: string, value: string) => `${name}="${plain(value)}"`,
  );
  for (const [sentinel, tag] of SENTINELS) html = html.replaceAll(sentinel, tag);
  // No encoded sentinel anywhere else either.
  return plain(html);
}
