/**
 * The files page's reader: a read-only CodeMirror instance over one file's
 * text, coloured by `@cmshiki/shiki` in the chat's github-dark family, with
 * the JS regex engine (no wasm).
 *
 * **This module is dynamically imported by the page** (`Reader.svelte`), so
 * everything statically imported here - CodeMirror, shiki, the grammars -
 * lands in one lazy chunk and the main bundle keeps them off its back. The
 * grammars are all static for the same reason made useful: shiki's lazy
 * per-language load made a language's first paint cost ~0.7s in the spike,
 * so the seat's own language set is warmed with the chunk instead.
 *
 * **Two layers per language, neither guessed.** Structure (folding) comes
 * from the CodeMirror org's own language package where one exists, loaded
 * on demand per language; the colouring is shiki's grammar either way. A
 * path this page has no language for draws as plain text - a wrong language
 * colours the wrong tokens, and a reader trusts colour more than they
 * should.
 */

import { shikiToCodeMirror } from '@cmshiki/shiki';
import { foldGutter, foldKeymap } from '@codemirror/language';
import { searchKeymap } from '@codemirror/search';
import { EditorState, RangeSet, StateEffect, StateField, type Extension } from '@codemirror/state';
import {
  Decoration,
  drawSelection,
  EditorView,
  GutterMarker,
  gutter,
  keymap,
  lineNumbers,
} from '@codemirror/view';
import { createHighlighterCore } from 'shiki/core';
import { createJavaScriptRegexEngine } from 'shiki/engine/javascript';

import bash from '@shikijs/langs/shellscript';
import css from '@shikijs/langs/css';
import go from '@shikijs/langs/go';
import html from '@shikijs/langs/html';
import ini from '@shikijs/langs/ini';
import javascript from '@shikijs/langs/javascript';
import json from '@shikijs/langs/json';
import jsx from '@shikijs/langs/jsx';
import kotlin from '@shikijs/langs/kotlin';
import markdown from '@shikijs/langs/markdown';
import python from '@shikijs/langs/python';
import rust from '@shikijs/langs/rust';
import sql from '@shikijs/langs/sql';
import svelte from '@shikijs/langs/svelte';
import toml from '@shikijs/langs/toml';
import tsx from '@shikijs/langs/tsx';
import typescript from '@shikijs/langs/typescript';
import xml from '@shikijs/langs/xml';
import yaml from '@shikijs/langs/yaml';
import githubDark from '@shikijs/themes/github-dark';

import { languageForPath } from './languages';

export interface ReaderHandle {
  /** Let the instance and its listeners go. */
  destroy(): void;
  /** Mark `line` as the reader's target - the tint and the gutter `+`. */
  targetLine(line: number | null): void;
}

const cmRust = async (): Promise<Extension> => (await import('@codemirror/lang-rust')).rust();
const cmJs = async (): Promise<Extension> =>
  (await import('@codemirror/lang-javascript')).javascript();
const cmTs = async (): Promise<Extension> =>
  (await import('@codemirror/lang-javascript')).javascript({ typescript: true });
const cmTsx = async (): Promise<Extension> =>
  (await import('@codemirror/lang-javascript')).javascript({ typescript: true, jsx: true });
const cmJsx = async (): Promise<Extension> =>
  (await import('@codemirror/lang-javascript')).javascript({ jsx: true });
const cmPython = async (): Promise<Extension> => (await import('@codemirror/lang-python')).python();
const cmGo = async (): Promise<Extension> => (await import('@codemirror/lang-go')).go();
const cmJson = async (): Promise<Extension> => (await import('@codemirror/lang-json')).json();
const cmYaml = async (): Promise<Extension> => (await import('@codemirror/lang-yaml')).yaml();
const cmCss = async (): Promise<Extension> => (await import('@codemirror/lang-css')).css();
const cmHtml = async (): Promise<Extension> => (await import('@codemirror/lang-html')).html();
const cmMarkdown = async (): Promise<Extension> =>
  (await import('@codemirror/lang-markdown')).markdown();
const cmSql = async (): Promise<Extension> => (await import('@codemirror/lang-sql')).sql();

/** The CodeMirror structure loaders, by extension - the folding layer,
 *  where the org ships a package (`languages.ts` says which do). */
const CM: Record<string, () => Promise<Extension>> = {
  rs: cmRust,
  ts: cmTs,
  mts: cmTs,
  cts: cmTs,
  tsx: cmTsx,
  js: cmJs,
  mjs: cmJs,
  cjs: cmJs,
  jsx: cmJsx,
  py: cmPython,
  go: cmGo,
  json: cmJson,
  yaml: cmYaml,
  yml: cmYaml,
  css: cmCss,
  html: cmHtml,
  htm: cmHtml,
  xml: cmHtml,
  sql: cmSql,
  md: cmMarkdown,
  markdown: cmMarkdown,
};

let shared: Promise<Awaited<ReturnType<typeof createHighlighterCore>>> | null = null;

/** The one highlighter, built on first use with every grammar warmed. */
function sharedHighlighter(): Promise<Awaited<ReturnType<typeof createHighlighterCore>>> {
  shared ??= createHighlighterCore({
    themes: [githubDark],
    langs: [
      rust,
      typescript,
      tsx,
      javascript,
      jsx,
      python,
      go,
      json,
      yaml,
      css,
      html,
      xml,
      sql,
      markdown,
      svelte,
      toml,
      ini,
      bash,
      kotlin,
    ],
    engine: createJavaScriptRegexEngine(),
  });
  return shared;
}

/** The reader's target line, as an effect the page's own controls dispatch. */
const setTarget = StateEffect.define<number | null>();

/** The `+` in the gutter of the target line: what a press there means. */
class DoorMarker extends GutterMarker {
  toDOM(): Node {
    const mark = document.createElement('span');
    mark.className = 'files-door';
    mark.textContent = '+';
    return mark;
  }
}

const doorMarker = new DoorMarker();

/** The target line's tint, and its gutter door; one field, read by both. */
const targetField = StateField.define<number | null>({
  create: () => null,
  update(value, tr) {
    for (const effect of tr.effects) {
      if (effect.is(setTarget)) return effect.value;
    }
    return value;
  },
  provide: (field) => [
    EditorView.decorations.compute([field], (state) => {
      const line = state.field(field);
      if (line === null || line > state.doc.lines) return Decoration.none;
      return Decoration.set([
        Decoration.line({ class: 'files-target' }).range(state.doc.line(line).from),
      ]);
    }),
    gutter({
      class: 'files-doors',
      markers: (view) => {
        const line = view.state.field(field);
        if (line === null || line > view.state.doc.lines) return RangeSet.empty;
        return RangeSet.of([doorMarker.range(view.state.doc.line(line).from)]);
      },
    }),
  ],
});

/** The line a mousedown landed on, or `null` for a press that missed lines. */
function lineOfEvent(view: EditorView, event: MouseEvent): number | null {
  const target = event.target;
  if (!(target instanceof Element)) return null;
  const lineEl = target.closest('.cm-line');
  if (lineEl === null) return null;
  const pos = view.posAtDOM(lineEl);
  return view.state.doc.lineAt(pos).number;
}

/**
 * Build the reader over `text`, returning the handle the page keeps.
 *
 * A press anywhere on a line is a reader asking to comment there: the line
 * becomes the target (tint + gutter `+`, so touch has a visible door) and
 * `oncomment` hears the line number. Read-only and texual: the state is
 * `readOnly` + non-editable, so no keypress reaches the document.
 */
export async function buildReader(
  parent: HTMLElement,
  text: string,
  path: string,
  oncomment?: (line: number) => void,
): Promise<ReaderHandle> {
  const lang = languageForPath(path);
  const cut = path.lastIndexOf('.');
  const ext = cut === -1 ? '' : path.slice(cut + 1).toLowerCase();
  const cm = lang?.cm === true ? CM[ext] : undefined;
  const structure = cm === undefined ? null : await cm();
  const { shiki } = await shikiToCodeMirror({
    lang: lang?.shiki ?? 'text',
    themes: { dark: 'github-dark' },
    defaultColor: 'dark',
    themeStyle: 'cm',
    engine: 'javascript',
    highlighter: await sharedHighlighter(),
  });

  const view = new EditorView({
    parent,
    state: EditorState.create({
      doc: text,
      extensions: [
        lineNumbers(),
        targetField,
        EditorState.readOnly.of(true),
        EditorView.editable.of(false),
        // Wrapped, never sideways: the sheet's own canon (a long line ends
        // at the box's edge), and what the sheet-wide census holds.
        EditorView.lineWrapping,
        drawSelection(),
        foldGutter(),
        keymap.of([...searchKeymap, ...foldKeymap]),
        structure === null ? [] : structure,
        shiki,
        EditorView.domEventHandlers({
          mousedown(event, view) {
            const line = lineOfEvent(view, event);
            if (line === null) return false;
            view.dispatch({ effects: setTarget.of(line) });
            oncomment?.(line);
            return false;
          },
        }),
      ],
    }),
  });

  return {
    destroy: () => view.destroy(),
    targetLine: (line) => {
      view.dispatch({ effects: setTarget.of(line) });
      if (line !== null && line <= view.state.doc.lines) {
        view.dispatch({ effects: EditorView.scrollIntoView(view.state.doc.line(line).from) });
      }
    },
  };
}
