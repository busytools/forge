/**
 * The assistant's prose, as the page draws it.
 *
 * **`markdown-it` rather than a renderer of our own.** The project's rule is
 * to reach for an upstream module and keep it maintained there, and markdown
 * is the last thing worth hand-rolling. It is the pick over the alternatives
 * for one reason that matters here: raw HTML is OFF by default, so a session's
 * prose can quote a page, a log or a file from someone else's repository and
 * the page will not run what it quotes. The Rust renderer this replaces did
 * the same thing by hand, escaping every `<` before parsing. An image is the
 * module's own tag rather than quoted HTML, so `withPanels` draws it as a row
 * naming it: the page does not fetch a host a message names either.
 *
 * `linkify` is off too: a URL in prose is already readable as text, and
 * turning every bare domain into a link is a change to the text rather than a
 * rendering of it.
 */

import MarkdownIt from 'markdown-it';
import type { MarkdownIt as Module, RendererRule } from 'markdown-it';

import { codePanel, fenceLanguage } from './code';

const READER = new MarkdownIt({
  html: false,
  linkify: false,
  typographer: false,
});

/**
 * The same renderer with soft breaks kept, for what the reader typed.
 *
 * The split the view this client replaced kept: its user path preserved
 * newlines and its assistant path did not - so a prompt's newlines survive as
 * breaks where a wrapped assistant line joins back into one. A prompt is
 * usually several lines, so this is the shape a person meets first. It is a
 * second instance rather than a flag on the shared one, because turning it on
 * there would change assistant prose, which that view did not do.
 */
const PROMPT = new MarkdownIt({
  html: false,
  linkify: false,
  typographer: false,
  breaks: true,
});

/**
 * A fenced block, as the page's own code panel: the fence's info string on the
 * header, the text under it coloured through the same lookup the tool-call
 * bodies use.
 *
 * The module's own `<pre><code>` is not that panel - the sheet's inline-code
 * rule boxes it and the fence's language only reaches the page as a class
 * nothing reads.
 */
function withPanels(module: Module): Module {
  // The module's own escaper, so a rule here escapes what the module escapes.
  const { escapeHtml } = module.utils;
  const fence: RendererRule = (tokens, index) => {
    const token = tokens[index];
    if (token === undefined) return '';
    const info = token.info.trim();
    return codePanel(info === '' ? null : info, fenceLanguage(info), token.content);
  };
  /**
   * An indented block, which is a code block with no fence and so no info
   * string to label it. Without this it keeps the module's own `<pre><code>`
   * and is drawn as the thing this rule exists to delete.
   */
  const indented: RendererRule = (tokens, index) => {
    const token = tokens[index];
    return token === undefined ? '' : codePanel(null, null, token.content);
  };
  /**
   * An image, named rather than fetched.
   *
   * The module's own rule draws an `<img>`, and the page would then request
   * whatever host the message named. A message is text someone else wrote, so
   * the page makes no request for it: the row names the picture and prints its
   * source as text, which leaves the reader something to read and to copy.
   */
  const image: RendererRule = (tokens, index, options, env, renderer) => {
    const token = tokens[index];
    if (token === undefined) return '';
    const alt = escapeHtml(renderer.renderInlineAsText(token.children ?? [], options, env));
    const src = escapeHtml(String(token.attrGet('src') ?? ''));
    const name = alt === '' ? src : alt;
    const source = alt === '' ? '' : `<span class="n">${src}</span>`;
    return `<span class="attrow"><svg class="ic gl"><use href="#i-read"></use></svg><span>${name}</span>${source}</span>`;
  };
  module.renderer.rules.fence = fence;
  module.renderer.rules.code_block = indented;
  module.renderer.rules.image = image;
  return module;
}

/**
 * Name every data cell with its column's heading, on the token itself.
 *
 * **A table that cannot fit is stacked, and a stacked cell needs its label.**
 * The header row is hidden in the stacked layout, so each cell carries the
 * heading it would have sat under; the sheet draws that in `::before`. Worked
 * on the token stream rather than in the renderer because a cell's column is
 * its place in the row, which the rendered HTML no longer knows.
 */
function withStackedTables(module: Module): Module {
  module.core.ruler.push('table_cell_labels', (state) => {
    const tokens = state.tokens;
    /** The heading of each column, by index, as the header row states it. */
    let headings: string[] = [];
    let inHead = false;
    let column = 0;
    for (let at = 0; at < tokens.length; at += 1) {
      const token = tokens[at];
      if (token === undefined) continue;
      if (token.type === 'table_open') {
        headings = [];
        inHead = false;
        column = 0;
      } else if (token.type === 'thead_open') {
        inHead = true;
      } else if (token.type === 'thead_close') {
        inHead = false;
      } else if (token.type === 'tr_open') {
        column = 0;
      } else if (token.type === 'th_open') {
        if (inHead) {
          const inline = tokens[at + 1];
          headings[column] =
            inline === undefined
              ? ''
              : module.renderer.renderInlineAsText(
                  inline.children ?? [],
                  module.options,
                  state.env,
                );
        }
        column += 1;
      } else if (token.type === 'td_open') {
        const heading = headings[column];
        if (heading !== undefined && heading !== '') token.attrSet('data-label', heading);
        column += 1;
      }
    }
  });
  return module;
}

withPanels(READER);
withPanels(PROMPT);
withStackedTables(READER);
withStackedTables(PROMPT);

/**
 * `text` as HTML, with everything that is not markdown left as text.
 *
 * `preserveLines` is for the reader's own block, where a newline someone typed
 * is a break they meant.
 */
export function renderProse(text: string, preserveLines = false): string {
  return (preserveLines ? PROMPT : READER).render(text);
}

/**
 * `text` as inline markdown: emphasis and code, no block of its own.
 *
 * For a row's own line, which is one line by the design's rule and cannot hold
 * a paragraph, a heading or a list. Same renderer and same escaping as the
 * body, so a row and its open body read the marks alike.
 */
export function renderInlineProse(text: string): string {
  return READER.renderInline(text);
}

/** One comment target of `text`, rendered on its own. */
interface ProseTarget {
  /** 1-based first source line the target came from. */
  line: number;
  /** 1-based last source line, inclusive. */
  end: number;
  /** The target's own HTML, through the same renderer as the body. */
  html: string;
}

/** The containers whose direct children are targets of their own: a list's
 *  items, a quote's paragraphs. A table stays one target. */
const SPLIT_CONTAINERS = new Set(['bullet_list_open', 'ordered_list_open', 'blockquote_open']);

/**
 * `text` as its comment targets, each rendered on its own.
 *
 * **The document viewer's target is finer than a top-level block.** A list of
 * ten states is ten things a reader may comment on, and a quote can carry
 * several paragraphs; a container's direct children become the targets, so
 * one anchor is one thing. The tokens carry the source span - markdown-it
 * records the lines every block token came from - and every slice opens and
 * closes together, which is what makes rendering the pieces draw what the
 * whole would.
 */
export function renderProseTargets(text: string): ProseTarget[] {
  const env = {};
  const tokens = READER.parse(text, env);
  const targets: ProseTarget[] = [];
  for (let at = 0; at < tokens.length; at += 1) {
    const token = tokens[at];
    if (token === undefined || token.level !== 0 || token.nesting < 0) continue;
    const map = token.map;
    if (map === null || map === undefined) continue;
    // A top-level block opens and closes (nesting 1); a fence, a rule or a
    // table stands alone (nesting 0).
    const close = token.nesting === 1 ? matchingClose(tokens, at) : at;
    if (close < 0) continue;
    if (!SPLIT_CONTAINERS.has(token.type)) {
      targets.push({
        line: map[0] + 1,
        end: map[1],
        html: READER.renderer.render(tokens.slice(at, close + 1), READER.options, env),
      });
      at = close;
      continue;
    }
    let ordinal = 0;
    for (let child = at + 1; child < close; child += 1) {
      const inner = tokens[child];
      if (inner === undefined || inner.level !== 1 || inner.nesting < 0) continue;
      const innerMap = inner.map;
      if (innerMap === null || innerMap === undefined) continue;
      // A leaf inside the container (a fence, a code block, a rule) opens
      // nothing, so it closes at itself; only a nesting child has a closer.
      const innerClose = inner.nesting === 0 ? child : matchingClose(tokens, child);
      if (innerClose < 0 || innerClose > close) continue;
      ordinal += 1;
      const body = READER.renderer.render(tokens.slice(child, innerClose + 1), READER.options, env);
      targets.push({
        line: innerMap[0] + 1,
        end: innerMap[1],
        html: wrapped(token, ordinal, body),
      });
      child = innerClose;
    }
    at = close;
  }
  return targets;
}

/** The index of the token that closes the block opened at `at`, by nesting. */
function matchingClose(tokens: { level: number; nesting: number }[], at: number): number {
  const open = tokens[at];
  if (open === undefined) return -1;
  let depth = 0;
  for (let index = at; index < tokens.length; index += 1) {
    const token = tokens[index];
    if (token === undefined || token.level !== open.level) continue;
    if (token.nesting === 1) depth += 1;
    else if (token.nesting === -1) {
      depth -= 1;
      if (depth === 0) return index;
    }
  }
  return -1;
}

/** A container child's own wrapper, so a one-item list still draws its marker
 *  and an ordered list keeps each item's number. */
function wrapped(
  container: { type: string; attrs: [string, string | number][] | null },
  ordinal: number,
  body: string,
): string {
  if (container.type === 'bullet_list_open') return `<ul>${body}</ul>`;
  if (container.type === 'blockquote_open') return `<blockquote>${body}</blockquote>`;
  const start = container.attrs?.find(([name]) => name === 'start')?.[1];
  const first = start === undefined ? 1 : Number(start);
  const at = first + ordinal - 1;
  return at === 1 ? `<ol>${body}</ol>` : `<ol start="${at}">${body}</ol>`;
}
