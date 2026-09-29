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

const READER = new MarkdownIt({
  html: false,
  linkify: false,
  typographer: false,
});

/** `text` as HTML, with everything that is not markdown left as text. */
export function renderProse(text: string): string {
  return READER.render(text);
}
