/**
 * Reading a tool result's own JSON.
 *
 * The wire hands a result as content blocks whose first readable text is the
 * answer the model was given; a family's card parses that text once, at the
 * fold, and never guesses twice. Every helper here answers `null`-ish rather
 * than throwing: a result this page cannot read is drawn as the raw text it
 * is, never dropped.
 */

/** The JSON inside a result's first text block, or null when there is none. */
export function parsedText(content: unknown): unknown {
  if (!Array.isArray(content)) return null;
  for (const held of content) {
    const block = held as { type?: unknown; text?: unknown } | null;
    if (block?.type !== 'text' || typeof block.text !== 'string') continue;
    try {
      return JSON.parse(block.text) as unknown;
    } catch {
      return null;
    }
  }
  return null;
}

/** The first text block's own words, for a result that is a sentence. */
export function firstText(content: unknown): string | null {
  if (!Array.isArray(content)) return null;
  for (const held of content) {
    const block = held as { type?: unknown; text?: unknown } | null;
    if (block?.type !== 'text' || typeof block.text !== 'string') continue;
    return block.text;
  }
  return null;
}

/**
 * The text blocks AFTER the first, in order.
 *
 * A card reads the first block as its answer; a result that carries more -
 * `review__list` appends a line naming the other branches that have reviews -
 * has still said all of it, so what the card does not read is drawn beside it
 * rather than dropped (rule 25).
 */
export function trailingTexts(content: unknown): string[] {
  const texts: string[] = [];
  let seen = 0;
  for (const held of Array.isArray(content) ? content : []) {
    const block = held as { type?: unknown; text?: unknown } | null;
    if (block?.type !== 'text' || typeof block.text !== 'string') continue;
    seen += 1;
    if (seen > 1) texts.push(block.text);
  }
  return texts;
}

/** `value` as a plain record, or an empty one for everything else. */
export function obj(value: unknown): Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return {};
  return value as Record<string, unknown>;
}

/** The string at `key`, or null for every other value. */
export function str(value: Record<string, unknown>, key: string): string | null {
  const held = value[key];
  return typeof held === 'string' ? held : null;
}
