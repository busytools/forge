/**
 * One call, as the row it draws.
 *
 * **A call's result arrives in a USER frame**, because that is the shape the
 * wire uses for it. It is not something the reader said, and drawing it as one
 * puts a command's output in a bubble attributed to the person who ran it.
 *
 * **What a call's body is comes from two places**, and both have to be read:
 * a mutation's diff is in the call's own INPUT, because the wire sends it with
 * the call, and everything else is in the result that answers it.
 */

import { languageOf } from './code';
import { isEdit, type CallStatus, type KindRow, rowOf } from './families';
import { headline, stripEscapes, toolName } from './text';

/** What a call's row opens on. */
export type CallBody =
  | { kind: 'text'; text: string }
  | { kind: 'diff'; old: string; new: string }
  | { kind: 'hunk'; header: string; lines: HunkLine[] }
  | { kind: 'image'; mime: string | null; uri: string | null };

/** One line of a hunk: context, removed, or added, with the number it holds on each side. */
export interface HunkLine {
  kind: 'ctx' | 'del' | 'add';
  text: string;
  /** Its number in the file as it was, or `null` for a line the change added. */
  old: number | null;
  /** And in the file as it is now, or `null` for a line the change removed. */
  new: number | null;
}

/** What a mutation's result says about itself, drawn as one line under its diff. */
export interface MutationMarks {
  /** How many hunks the change drew, which is what says there is a line to draw at all. */
  hunks: number;
  /** How many lines it added, and how many it removed: the size of the change. */
  added: number;
  removed: number;
  /** Whether the edit replaced every match rather than the first. */
  all: boolean;
  /** Whether the file changed outside this edit, which the CLI reports on the result. */
  outside: boolean;
}

/**
 * What the wire reports about a call it runs as a TASK, backgrounded or not.
 *
 * The launch result of a backgrounded call says only that the command started,
 * so a fold that settles the call from its result draws a running command as a
 * finished one. The task frames are what say otherwise: `task_started` opens
 * the call's own clock and says whether it outlives its turn, `task_updated`
 * carries the ending, and `task_notification` carries what the harness said.
 */
export interface BackgroundTask {
  status: CallStatus;
  /**
   * Whether the wire said this task outlives its turn.
   *
   * **The note is only for that case.** A foreground call's own result already
   * carries what it came back with, and a dispatched agent's report is the
   * call's own body - drawing the harness's summary under either one repeats
   * what the row already says, once in the row's title and once in the line.
   */
  backgrounded: boolean;
  /**
   * What the harness said when the task ended, when it said anything.
   *
   * The tone is the status word's own reading - green for a task that
   * finished, red for one that failed or was killed - and `null` for a word
   * this page does not know, which is not a failure and is not a success.
   */
  note: { text: string; tone: 'sum' | 'fail' | null } | null;
}

/** One call, as its own row. */
export interface ToolLeaf {
  /** The `tool_use` id the wire gave it, which is what its result names. */
  id: string;
  /** The class this call belongs to, which a row picks its glyph from. */
  row: KindRow;
  /** The CLI's own name for the tool. */
  name: string;
  /** What the row draws as the call's name. */
  title: string;
  /**
   * The command the call ran, when it ran one.
   *
   * Separate from the title because a call given a description is named by
   * that instead, and the command it actually ran would then appear nowhere:
   * the body leads with it, so the whole of it stays readable even where the
   * row's own title is capped.
   */
  command: string | null;
  status: CallStatus;
  /**
   * What the harness said when a BACKGROUNDED call ended: drawn as the last
   * line of the box the call's result drew, and `null` for every other call.
   */
  note: { text: string; tone: 'sum' | 'fail' | null } | null;
  /** What the row opens on. Empty for a call that has not come back yet. */
  body: CallBody[];
  /**
   * What the result said about a mutation, or `null` for every other call.
   *
   * Drawn as one line under the diff, and only where a mark is true: a reader
   * acts on "every match" and on "changed outside this edit", and the default
   * of both is nothing to say.
   */
  mutation: MutationMarks | null;
  /**
   * The skill's own markdown, for a `Skill` call, or null for every other
   * call.
   *
   * The CLI injects a skill's body as a user frame after the call that loaded
   * it, and the fold attaches it here so the row that names the skill is the
   * row that opens onto it - the launch result ("Launching skill: ...") is the
   * CLI talking to itself, and the body is what the skill is.
   */
  skill: string | null;
  /**
   * The image a call's result carried, or null for every other call.
   *
   * A `Read` of a screenshot comes back as an image block in its result, and
   * the row that read it is where a reader looks for it. Held as the wire's
   * own base64 so the page can draw it from a data URL, and drawn only when
   * the row is open - the decode is real, and a column of closed rows must
   * not pay it.
   */
  image: { mime: string; data: string } | null;
  /**
   * The harness's own line about that image ("original WxH, displayed at ..."),
   * which arrives as a user frame right after the result. The fold attaches
   * it here so it draws as the picture's caption rather than as a turn of the
   * reader's own.
   */
  imageNote: string | null;
}

/**
 * Whether a call's body is drawn without being asked for: a mutation's diff,
 * while it is small enough to draw.
 *
 * **A very large diff is not drawn open.** Laying one out costs WebKit a full
 * pass over it on every dirty, which pins the renderer on the seat holding it
 * - a real diff of ~245,000px froze the page that drew it, its loading line
 * included. A row over the bound starts closed; one click still opens it, and
 * nothing is dropped.
 */
export function opensByDefault(name: string, body: CallBody[]): boolean {
  return isEdit(name) && drawnDiffLines(body) <= OPEN_DIFF_LINES;
}

/** The most diff rows a mutation's row draws without being asked. */
const OPEN_DIFF_LINES = 2000;

/**
 * The rows a body's diff pieces would draw, counted as the ROW draws them.
 *
 * **Rows, not newlines.** The drawing wraps, so a file of a few enormous lines
 * draws hundreds of thousands of pixels: one real diff carried a 441,094
 * character line, which a newline count reads as one. Counting the wrapped
 * rows is the unit the layout cost is actually in.
 */
function drawnDiffLines(body: CallBody[]): number {
  let lines = 0;
  for (const piece of body) {
    if (piece.kind === 'diff') {
      lines += wrappedRows(piece.old) + wrappedRows(piece.new);
    } else if (piece.kind === 'hunk') {
      for (const line of piece.lines) lines += wrappedRows(line.text);
    }
  }
  return lines;
}

/** The rows a block of text draws once wrapped, at the diff's own text column. */
function wrappedRows(text: string): number {
  if (text === '') return 0;
  let rows = 0;
  for (const line of text.split('\n')) {
    rows += Math.max(1, Math.ceil(line.length / DIFF_COLUMNS));
  }
  return rows;
}

/** Roughly the diff's text column at a wide window; the bound only needs its order. */
const DIFF_COLUMNS = 76;

/** Whether a call's body is a source file the page colours. */
export function readsSource(name: string): boolean {
  return name === 'Read';
}

/** A field of a JSON object, when it is a string. */
function field(value: unknown, key: string): string | null {
  const held = (value as Record<string, unknown> | null)?.[key];
  return typeof held === 'string' ? held : null;
}

/** A block, as one of the shapes the wire uses. */
export interface Block {
  type?: unknown;
  text?: unknown;
  id?: unknown;
  name?: unknown;
  input?: unknown;
  tool_use_id?: unknown;
  content?: unknown;
  is_error?: unknown;
  media_type?: unknown;
  url?: unknown;
  source?: unknown;
  prompt?: unknown;
  commandMode?: unknown;
  thinking?: unknown;
}

/** The blocks of a frame's content, for a frame that carries any. */
export function blocksOf(content: unknown): Block[] {
  return Array.isArray(content) ? (content as Block[]) : [];
}

/**
 * What a call's result recorded, from the shapes a tool result arrives in.
 *
 * The content is a string for most tools and a block array for a few, and both
 * are read here: a result drawn as nothing is a row that expands to an empty
 * box.
 */
export function bodyOf(content: unknown): CallBody[] {
  if (typeof content === 'string') {
    const text = stripEscapes(content);
    return text.trim() === '' ? [] : [{ kind: 'text', text }];
  }
  const out: CallBody[] = [];
  for (const block of blocksOf(content)) {
    if (block.type === 'text' && typeof block.text === 'string') {
      const text = stripEscapes(block.text);
      if (text.trim() !== '') out.push({ kind: 'text', text });
    }
    if (block.type === 'image') {
      // The wire nests both under `source`, which is the shape a user turn's
      // own attachment carries: read off the block they are always absent, and
      // a tool's image draws with no mime at all.
      out.push({
        kind: 'image',
        mime: field(block.source, 'media_type'),
        uri: field(block.source, 'url'),
      });
    }
  }
  return out;
}

/**
 * The diff a mutation carries in its own input, which is where the wire puts
 * it.
 *
 * **The four mutations do not share one input shape, and each writes its
 * sides differently.** `Write` carries the whole FILE rather than a change to
 * it, so its content is the ADDED side and nothing was removed: read the
 * other way round it draws a file the session just wrote as a file it
 * deleted, in a card that opens without being clicked. `MultiEdit` carries a
 * list of edits, one hunk each. `NotebookEdit` carries a cell's new source
 * and no old side at all.
 *
 * A mutation whose input this page cannot read draws no diff rather than an
 * empty card, which is what an unread shape would otherwise leave open on
 * screen.
 */
export function diffsOf(name: string, input: unknown): CallBody[] {
  if (!isEdit(name)) return [];
  const path = field(input, 'file_path') ?? field(input, 'notebook_path');
  if (path === null) return [];

  if (name === 'Write') return created(field(input, 'content') ?? '');
  if (name === 'NotebookEdit') return created(field(input, 'new_source') ?? '');
  if (name === 'MultiEdit') {
    const edits = (input as { edits?: unknown } | null)?.edits;
    if (!Array.isArray(edits)) return [];
    return edits.flatMap((edit) =>
      hunk(field(edit, 'old_string') ?? '', field(edit, 'new_string') ?? ''),
    );
  }
  return hunk(field(input, 'old_string') ?? '', field(input, 'new_string') ?? '');
}

/**
 * The CLI's own hunks for a mutation, which is where the change's position and
 * its context are.
 *
 * **The result carries them and this page used to ignore them**: one
 * `structuredPatch` entry per hunk, each with the range it covers before and
 * after and the lines themselves, a space for context, `-` for removed and `+`
 * for added. Without them a diff is two sides with nothing saying where in the
 * file they sit - which is what the row drew until now.
 *
 * A result that carries no patch (a tool the CLI does not describe this way, a
 * transcript row written before it did) leaves the input's own two sides to
 * draw, which is what `diffsOf` is for.
 */
export function hunksOf(record: unknown): CallBody[] {
  const patch = (record as { structuredPatch?: unknown } | null)?.structuredPatch;
  if (!Array.isArray(patch)) return [];
  const out: CallBody[] = [];
  for (const entry of patch) {
    const hunk = entry as {
      oldStart?: unknown;
      oldLines?: unknown;
      newStart?: unknown;
      newLines?: unknown;
      lines?: unknown;
    };
    const lines = hunkLines(hunk.lines, Number(hunk.oldStart ?? 1), Number(hunk.newStart ?? 1));
    if (lines.length === 0) continue;
    out.push({ kind: 'hunk', header: headerOf(hunk), lines });
  }
  return out;
}

/**
 * The lines of one hunk, each read by the character the wire prefixes it with
 * and numbered on both sides.
 *
 * **The numbers are counted from the hunk's own ranges**, which is what makes
 * two hunks in one file readable: without them each block is a change with no
 * way to see how far it sits from the one above it, and the header's range is
 * the only thing saying where either is. A removed line holds its number in
 * the file as it was and none in the file as it is; an added line the other way
 * round; a context line holds both, which is what holds the two sides in step.
 */
function hunkLines(raw: unknown, oldStart: number, newStart: number): HunkLine[] {
  if (!Array.isArray(raw)) return [];
  const out: HunkLine[] = [];
  let old = oldStart;
  let next = newStart;
  for (const line of raw) {
    if (typeof line !== 'string' || line === '') continue;
    // By index, not by spreading the line: the spread costs one string per character.
    const mark = line.charAt(0);
    const kind = mark === '-' ? 'del' : mark === '+' ? 'add' : 'ctx';
    out.push({
      kind,
      text: kind === 'ctx' ? line : line.slice(1),
      old: kind === 'add' ? null : old,
      new: kind === 'del' ? null : next,
    });
    if (kind !== 'add') old += 1;
    if (kind !== 'del') next += 1;
  }
  return out;
}

/** Where a hunk sits: `@@ -30,7 +30,9 @@`, which is the shape the terminal prints too. */
function headerOf(hunk: {
  oldStart?: unknown;
  oldLines?: unknown;
  newStart?: unknown;
  newLines?: unknown;
}): string {
  const num = (value: unknown): string => (typeof value === 'number' ? String(value) : '?');
  return `@@ -${num(hunk.oldStart)},${num(hunk.oldLines)} +${num(hunk.newStart)},${num(hunk.newLines)} @@`;
}

/**
 * What a mutation's body draws: the CLI's hunks where the result carries them,
 * and the call's own two sides where it does not.
 */
function mutationBody(name: string, input: unknown, record: unknown): CallBody[] {
  if (!isEdit(name)) return [];
  const hunks = hunksOf(record);
  return hunks.length > 0 ? hunks : diffsOf(name, input);
}

/**
 * What a mutation's row says under its diff, or `null` for a call that is not a
 * mutation.
 *
 * **Counted from what the row actually drew**, so the figures cannot disagree
 * with the change above them: the hunks the result carried, or the two sides
 * the fallback drew as one.
 */
function marksOf(
  name: string,
  input: unknown,
  body: CallBody[],
  record: unknown,
): MutationMarks | null {
  if (!isEdit(name)) return null;
  let hunks = 0;
  let added = 0;
  let removed = 0;
  for (const part of body) {
    if (part.kind === 'hunk') {
      hunks += 1;
      for (const line of part.lines) {
        if (line.kind === 'add') added += 1;
        else if (line.kind === 'del') removed += 1;
      }
    } else if (part.kind === 'diff') {
      hunks += 1;
      if (part.new !== '') added += part.new.split('\n').length;
      if (part.old !== '') removed += part.old.split('\n').length;
    }
  }
  const all = (input as { replace_all?: unknown } | null)?.replace_all === true;
  const outside = (record as { userModified?: unknown } | null)?.userModified === true;
  return { hunks, added, removed, all, outside };
}

/**
 * A file the call created, as git and GitHub draw one.
 *
 * **The range is knowable even though the wire sends no patch for it**: a
 * create goes from line zero to however many lines it wrote, which is the one
 * first hunk `@@ -0,0 +1,N @@`, and every line of it is an addition. The
 * alternative is what the row drew before - the whole file as a bare run of
 * plus lines with nothing saying what it was or where it started.
 */
function created(content: string): CallBody[] {
  if (content.trim() === '') return [];
  const lines = content.split('\n');
  // A trailing newline ends the last line rather than starting an empty one.
  if (lines[lines.length - 1] === '') lines.pop();
  return [
    {
      kind: 'hunk',
      header: `@@ -0,0 +1,${lines.length} @@`,
      lines: lines.map((text, at) => ({ kind: 'add' as const, text, old: null, new: at + 1 })),
    },
  ];
}

/**
 * One hunk, or none when it says nothing: an empty old side and an empty new
 * one is no change.
 *
 * **The path is checked above and not carried here.** The row's own title is
 * the file, whole, so a hunk naming it again drew the same path twice on one
 * row; what is left of the check is that a mutation with no path in its input
 * draws nothing at all.
 */
function hunk(old: string, added: string): CallBody[] {
  if (old.trim() === '' && added.trim() === '') return [];
  return [{ kind: 'diff', old, new: added }];
}

/**
 * What names a call on its row: its input where that says something, its tool otherwise.
 *
 * **The path as the wire gave it rather than cut against the working tree.** A
 * mutation's row draws the same file twice - its title, and the header of the
 * diff under it - so shortening one of them is the row disagreeing with itself
 * about which file it is. The terminal puts the whole of it on the row (its
 * `create_tool_call` builds `Edit /Users/.../home.css`), which is the reference
 * this follows.
 */
export function titleOf(name: string, input: unknown): string {
  const said = headline(name, input);
  return said === name ? toolName(name) : said;
}

/**
 * One call, from the wire's own fields.
 *
 * `result` is what a later frame recorded for it, when one has arrived: a call
 * that has not come back yet still carries what its own input says, because an
 * edit's diff is in the call rather than in the answer.
 *
 * `task` settles the call when the wire gave it an ending, and holds it open
 * only where the wire said the task OUTLIVES its turn: such a call's launch
 * result says only that the command started, so it has to draw as running
 * however clean that result was. A foreground task still running is settled by
 * its result and the turn's verdict, like a call with no task frame at all -
 * so an abandoned foreground call draws failed rather than running.
 *
 * `abandoned` is the turn's verdict on a call it never answered: a turn that
 * failed is a turn whose open calls never get a result, and they draw as
 * failed rather than as still running - which is what the terminal does with
 * them on the same frame.
 */
export function leafOf(
  id: string,
  name: string,
  input: unknown,
  result: Block | undefined,
  record: unknown = undefined,
  task: BackgroundTask | undefined = undefined,
  abandoned = false,
): ToolLeaf {
  const body = mutationBody(name, input, record);
  const settled =
    task !== undefined && (task.backgrounded || task.status !== 'in_progress')
      ? task.status
      : settledBy(result, abandoned);
  return {
    id,
    row: rowOf(name),
    name,
    title: titleOf(name, input),
    command: field(input, 'command')?.trim() || null,
    status: settled,
    note: task?.backgrounded === true ? task.note : null,
    body: drawnBody(name, body, result),
    mutation: marksOf(name, input, body, record),
    skill: null,
    image: imageOf(result),
    imageNote: null,
  };
}

/** The image a result carried, as the wire's own mime and base64, or null. */
function imageOf(result: Block | undefined): { mime: string; data: string } | null {
  const inner = result?.content;
  if (!Array.isArray(inner)) return null;
  for (const held of inner) {
    const block = held as { type?: unknown; source?: unknown } | null;
    if (block?.type !== 'image') continue;
    const source = block.source as { media_type?: unknown; data?: unknown } | null;
    if (typeof source?.data !== 'string' || source.data === '') continue;
    return {
      mime: typeof source.media_type === 'string' ? source.media_type : 'image/png',
      data: source.data,
    };
  }
  return null;
}

/**
 * What a call's body draws, which for a mutation is its diff and nothing else.
 *
 * The result's own text repeats the diff's path and carries the CLI's note on
 * the file state, which is an instruction to the model rather than anything a
 * reader acts on; the terminal draws the diff and returns there for both `Edit`
 * and `Write` (`build_tool_result_fields`). A failed mutation is the other way
 * round: its text is the reason nothing changed - "String to replace not found
 * in file", "File has not been read yet" - and the terminal keeps it (its guard
 * is `if !is_error`), so a row that dropped it would say the edit failed and
 * never say why. Where there is no diff the text is all there is, and either
 * way the call still settles on its result.
 */
function drawnBody(name: string, body: CallBody[], result: Block | undefined): CallBody[] {
  if (result === undefined) return body;
  const answered = bodyOf(result.content);
  if (result.is_error !== true && isEdit(name) && body.some((part) => part.kind !== 'text')) {
    return body;
  }
  return [...body, ...answered];
}

/** What settles a call with no task frame of its own: its result, or the turn that never sent one. */
function settledBy(result: Block | undefined, abandoned: boolean): CallStatus {
  if (result === undefined) return abandoned ? 'failed' : 'pending';
  return result.is_error === true ? 'failed' : 'completed';
}

/** Whether a call's body draws as a source file: a read that named one this page knows. */
export function languageFor(leaf: ToolLeaf): string | null {
  return readsSource(leaf.name) ? languageOf(leaf.title) : null;
}
