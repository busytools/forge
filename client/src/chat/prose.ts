/**
 * The assistant's prose, as the page draws it.
 *
 * **`markdown-it` rather than a renderer of our own.** The project's rule is
 * to reach for an upstream module and keep it maintained there, and markdown
 * is the last thing worth hand-rolling. It is the pick over the alternatives
 * for one reason that matters here: raw HTML is OFF by default, so a session's
 * prose can quote a page, a log or a file from someone else's repository and
 * the page will not run what it quotes. The Rust renderer this replaces did
 * the same thing by hand, escaping every `<` before parsing.
 *
 * `linkify` is off too: a URL in prose is already readable as text, and
 * turning every bare domain into a link is a change to the text rather than a
 * rendering of it.
 */

import MarkdownIt from 'markdown-it';

import { codePanel, fenceLanguage } from './code';

const READER = new MarkdownIt({
  html: false,
  linkify: false,
  typographer: false,
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
READER.renderer.rules.fence = (tokens, index) => {
  const token = tokens[index];
  if (token === undefined) return '';
  const info = token.info.trim();
  return codePanel(info === '' ? null : info, fenceLanguage(info), token.content);
};

/**
 * An indented block, which is a code block with no fence and so no info string
 * to label it. Without this it keeps the module's own `<pre><code>` and is
 * drawn as the thing this rule exists to delete.
 */
READER.renderer.rules.code_block = (tokens, index) => {
  const token = tokens[index];
  return token === undefined ? '' : codePanel(null, null, token.content);
};

/** `text` as HTML, with everything that is not markdown left as text. */
export function renderProse(text: string): string {
  return READER.render(text);
}
