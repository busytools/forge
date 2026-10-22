<script lang="ts">
  import { untrack } from 'svelte';

  import {
    diffLineHtml,
    pairedLineTints,
    pairedTints,
    type ColourToken,
    type Span,
  } from '../format/diff';
  import { formatOf } from '../format/policy';
  import type { CallBody } from './leaves';

  /**
   * A call's patch: the hunks with their line numbers and marks, GitHub's
   * tinted grounds, the file's grammar over every line, and the changed words
   * of a paired removal and addition tinted darker.
   *
   * **Plain first, coloured when the grammar lands**, the code panel's own
   * rule: the plain pass draws every word, and shiki's tokens replace it
   * without changing the structure.
   */
  type Piece = Extract<CallBody, { kind: 'diff' } | { kind: 'hunk' }>;

  let {
    pieces,
    path,
    added = false,
    bare = false,
  }: {
    pieces: Piece[];
    /** The file the patch is in, which names the grammar. */
    path: string;
    /** Whether the change only adds, so no old-number column draws. */
    added?: boolean;
    /** Whether the change is the call's own two sides, which have no positions. */
    bare?: boolean;
  } = $props();

  interface Row {
    kind: 'ctx' | 'del' | 'add';
    text: string;
    old: string;
    next: string;
    tints: Span[];
  }

  interface Block {
    header: string | null;
    rows: Row[];
    /** The rows of each side, as the grammar is tokenised over each file. */
    oldRows: number[];
    newRows: number[];
  }

  const blocks = $derived.by((): Block[] => {
    const out: Block[] = [];
    for (const piece of pieces) {
      if (piece.kind === 'hunk') {
        const tints = pairedTints(piece.lines);
        const rows = piece.lines.map((line, at): Row => ({
          kind: line.kind,
          text: line.text,
          old: line.old === null ? '' : String(line.old),
          next: line.new === null ? '' : String(line.new),
          tints: tints[at] ?? [],
        }));
        out.push({
          header: piece.header,
          rows,
          oldRows: rows.flatMap((row, at) => (row.kind === 'add' ? [] : [at])),
          newRows: rows.flatMap((row, at) => (row.kind === 'del' ? [] : [at])),
        });
        continue;
      }
      const tints = pairedLineTints(piece.old, piece.new);
      const rows: Row[] = [];
      const oldRows: number[] = [];
      const newRows: number[] = [];
      if (piece.old !== '') {
        for (const [at, text] of piece.old.split('\n').entries()) {
          oldRows.push(rows.length);
          rows.push({ kind: 'del', text, old: '', next: '', tints: tints.old[at] ?? [] });
        }
      }
      if (piece.new !== '') {
        for (const [at, text] of piece.new.split('\n').entries()) {
          newRows.push(rows.length);
          rows.push({ kind: 'add', text, old: '', next: '', tints: tints.new[at] ?? [] });
        }
      }
      out.push({ header: null, rows, oldRows, newRows });
    }
    return out;
  });

  const language = $derived.by((): string | null => {
    const format = formatOf(path);
    return format.kind === 'code' ? format.language : null;
  });

  /**
   * The pieces' own text, as the signal a re-tokenise hangs off.
   *
   * **A string, not the pieces.** The fold hands the row a new object on
   * every frame, so an effect keyed on `pieces` would re-run shiki per frame
   * of a streaming turn; the text is what actually changes.
   */
  const signature = $derived(
    pieces
      .map((piece) =>
        piece.kind === 'diff'
          ? `d:${piece.old}\u0000${piece.new}`
          : `h:${piece.header}\u0000${piece.lines.map((line) => line.kind + line.text).join('\u0000')}`,
      )
      .join('\u0001'),
  );

  /** One token list per row of each block, or `null` while the plain pass stands. */
  let tokens = $state<(readonly ColourToken[] | null)[][] | null>(null);

  $effect(() => {
    const id = language;
    void signature;
    const drawn = untrack(() => blocks);
    tokens = null;
    if (id === null) return;
    let live = true;
    // A grammar that cannot load leaves the plain pass, which carries every
    // word: a colouring failure is not a lost patch.
    void import('../format/highlight')
      .then(async ({ tokenisedLines }) => {
        const held: (readonly ColourToken[] | null)[][] = [];
        for (const block of drawn) {
          const rowTokens: (readonly ColourToken[] | null)[] = Array.from(
            { length: block.rows.length },
            () => null,
          );
          const oldText = block.oldRows.map((at) => block.rows[at]?.text ?? '').join('\n');
          const newText = block.newRows.map((at) => block.rows[at]?.text ?? '').join('\n');
          const oldTokens = block.oldRows.length === 0 ? [] : await tokenisedLines(id, oldText);
          const newTokens = block.newRows.length === 0 ? [] : await tokenisedLines(id, newText);
          block.oldRows.forEach((at, index) => {
            rowTokens[at] = oldTokens[index] ?? null;
          });
          block.newRows.forEach((at, index) => {
            rowTokens[at] = newTokens[index] ?? null;
          });
          held.push(rowTokens);
        }
        if (live) tokens = held;
      })
      .catch(() => undefined);
    return () => {
      live = false;
    };
  });

  /** One row's markup, from its tokens when they have landed and escaping otherwise. */
  function rowBody(row: Row, block: number, at: number): string {
    return diffLineHtml(row.text, row.tints, tokens?.[block]?.[at] ?? null);
  }

  /** `text` escaped: a hunk header is the file's own words. */
  function escaped(text: string): string {
    return text.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
  }

  /** The mark a row carries beside its number. */
  function mark(row: Row): string {
    return row.kind === 'del' ? '\u{2212}' : row.kind === 'add' ? '+' : '';
  }

  /** The row class the sheet names a line by: `d`, `a`, or `ctx`. */
  function rowClass(row: Row): string {
    return row.kind === 'del' ? 'd' : row.kind === 'add' ? 'a' : 'ctx';
  }

  /** One row, as the patch box draws it. */
  function rowHtml(row: Row, block: number, at: number): string {
    if (bare) {
      return `<div class="ln ${rowClass(row)}"><span class="n">${mark(row)}</span><span class="l">${rowBody(row, block, at)}</span></div>`;
    }
    return `<div class="ln ${rowClass(row)}"><span class="on">${row.old}</span><span class="nn">${row.next}</span><span class="n">${mark(row)}</span><span class="l">${rowBody(row, block, at)}</span></div>`;
  }

  const html = $derived.by((): string =>
    blocks
      .map((block, at) => {
        const header = block.header === null ? '' : `<div class="h">${escaped(block.header)}</div>`;
        const rows = block.rows.map((row, index) => rowHtml(row, at, index)).join('');
        return header + rows;
      })
      .join(''),
  );
</script>

<!-- The patch box: the escaped body the plain pass drew, or the same rows with
     the grammar's tokens in place. Both come from the same builder. -->
<!-- eslint-disable-next-line svelte/no-at-html-tags -->
<div class="dif" class:added class:bare>{@html html}</div>
