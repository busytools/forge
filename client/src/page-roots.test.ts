/**
 * The sheet's page roots are namespaced to their page: `<page>-page`.
 *
 * **The incident.** The files page's root was `.files`, the same name the
 * home puts on its changed-file count (`.fleet-where .files`). The page's
 * `height: 100dvh` reached a fleet row and stretched it to 954px in v1.1.20.
 * The component-scoped guard in `chat/Queue.test.ts` reads one component's
 * mounted subtree and cannot see a page root reaching another surface.
 *
 * **Checked at the definition, not at every application.** The seven page
 * routes are enumerated here; each root is the page's name plus `-page`, the
 * sheet must scope rules BY the root (a selector whose subject is the root,
 * not a mention of it), and the route union must be covered so a new page
 * cannot arrive unnamed. A new page starts namespaced: there is no exception
 * list.
 *
 * **The branch scan is per root kind**: a page whose root is its `<main>`
 * has every `<main>` in its component checked, because those are its
 * branches; a page whose root is a wrapper must draw exactly one top-level
 * classed element, so a second root is caught. A page that draws root
 * branches another way is not covered by this check yet.
 *
 * **What it cannot prove**: that a rename's descendants and component-scoped
 * styles moved with it. This check proves the class name and a subject
 * selector agree, not that the page still looks right - a rename cut owes
 * the descendant sweep and the look on top of it.
 */

import { readFileSync } from 'node:fs';

import { JSDOM } from 'jsdom';
import { parse } from 'svelte/compiler';
import { describe, expect, it } from 'vitest';

/** One page's root: the route it draws, the components that draw it, its class. */
interface PageRoot {
  /** The page's name, which its root must be namespaced to. */
  page: string;
  /**
   * Every component that draws a root branch: the models page's loaded
   * state is ModelsBody, its loading and refused states Models.
   */
  components: string[];
  /** The class on the page's root element, as the sheet spells it. */
  root: string;
  /**
   * Where the page's root branches sit: the components' `<main>`s (every
   * branch is a root), or the one top-level element they open with.
   */
  rootTag: 'main' | 'first';
}

/** The seven page routes and their roots. */
const PAGES: PageRoot[] = [
  {
    page: 'board',
    components: ['src/board/Board.svelte'],
    root: 'board-page',
    rootTag: 'main',
  },
  {
    page: 'doc',
    components: ['src/doc/Document.svelte'],
    root: 'doc-page',
    rootTag: 'main',
  },
  {
    page: 'extensions',
    components: ['src/extensions/Extensions.svelte'],
    root: 'extensions-page',
    rootTag: 'main',
  },
  {
    page: 'files',
    components: ['src/files/Files.svelte'],
    root: 'files-page',
    rootTag: 'first',
  },
  {
    page: 'models',
    components: ['src/models/Models.svelte', 'src/models/ModelsBody.svelte'],
    root: 'models-page',
    rootTag: 'main',
  },
  {
    page: 'review',
    components: ['src/review/Review.svelte'],
    root: 'review-page',
    rootTag: 'main',
  },
  {
    page: 'session',
    components: ['src/session/Session.svelte'],
    root: 'session-page',
    rootTag: 'first',
  },
];

/**
 * Routes with no page-scoped root of their own, each for a stated reason:
 * connect and the dev fixture draw under `.wrap`, home's page class is the
 * shell's `.wrap`, and `notFound` draws `.wrap`. A new route in neither set
 * fails the coverage check.
 */
const SHARED_ROOT_ROUTES = new Set(['connect', 'fixture', 'home', 'notFound']);

/** The roots that are not `<page>-page`. */
function namingProblems(pages: PageRoot[]): string[] {
  return pages
    .filter((page) => page.root !== `${page.page}-page`)
    .map(
      (page) =>
        `.${page.root} is not namespaced to the ${page.page} page; ` +
        `a root is .${page.page}-page`,
    );
}

/**
 * The routes a `Route` union serves, read from the union only.
 *
 * **The union, not the file**: `parseRoute` returns `{ name: ... }` objects
 * too, so a whole-file scan would still count a route the union no longer
 * serves. Names are read in any case and in either quote style
 * (`notFound` is camelCase).
 */
function namesInUnion(source: string): Set<string> {
  const marker = 'export type Route';
  const start = source.indexOf(marker);
  const end = source.indexOf('\nexport ', start + marker.length);
  if (start < 0 || end < 0) throw new Error('the route union was not found');
  const union = source.slice(start, end);
  return new Set(
    [...union.matchAll(/name:\s*(['"])([A-Za-z0-9_-]+)\1/g)]
      .map((match) => match[2])
      .filter((name): name is string => name !== undefined),
  );
}

/** [`namesInUnion`] over the tree's own routes. */
function routeNames(): Set<string> {
  return namesInUnion(readFileSync('src/routes.ts', 'utf8'));
}

/** A Svelte component's markup: after the script, before the style, comments out. */
function markupOf(source: string): string {
  const scriptEnd = source.lastIndexOf('</script>');
  const start = scriptEnd === -1 ? 0 : scriptEnd + '</script>'.length;
  const styleAt = source.indexOf('<style', start);
  const markup = source.slice(start, styleAt === -1 ? source.length : styleAt);
  return markup.replace(/<!--[\s\S]*?-->/g, ' ');
}

/** The part of a Svelte AST this check reads. */
interface AstNode {
  type: string;
  name?: string;
  attributes?: { type: string; name?: string; value?: unknown }[];
  consequent?: { nodes?: AstNode[] };
  alternate?: { nodes?: AstNode[] };
  body?: { nodes?: AstNode[] };
}

/** The class tokens a static `class` attribute carries. */
function classAttributeTokens(attribute: { value?: unknown }): string[] {
  const parts = Array.isArray(attribute.value) ? attribute.value : [];
  return parts
    .map((part) => {
      const held = part as { type?: string; data?: unknown };
      return held.type === 'Text' && typeof held.data === 'string' ? held.data : null;
    })
    .filter((text): text is string => text !== null)
    .flatMap((text) => text.split(/\s+/))
    .filter((token) => token !== '');
}

/**
 * Every classed element at the top of a component's fragment, in order.
 *
 * **The component is parsed, not pattern-matched**: a top-level conditional
 * branch's element is a top-level element too, so a second root behind an
 * `{#if}` is caught. Blocks are descended into; elements are not.
 */
function topLevelClassTokens(source: string): string[][] {
  const ast = parse(source, { modern: true }) as unknown as {
    fragment: { nodes: AstNode[] };
  };
  const found: string[][] = [];
  const visit = (nodes: AstNode[]): void => {
    for (const node of nodes) {
      if (node.type === 'RegularElement') {
        const classes = (node.attributes ?? [])
          .filter((attribute) => attribute.type === 'Attribute' && attribute.name === 'class')
          .flatMap((attribute) => classAttributeTokens(attribute));
        if (classes.length > 0) found.push(classes);
        continue;
      }
      if (node.type === 'IfBlock') {
        if (node.consequent?.nodes !== undefined) visit(node.consequent.nodes);
        if (node.alternate?.nodes !== undefined) visit(node.alternate.nodes);
        continue;
      }
      if (node.type === 'EachBlock' && node.body?.nodes !== undefined) visit(node.body.nodes);
    }
  };
  visit(ast.fragment.nodes);
  return found;
}

/** The page component's root classes, one entry per root branch. */
function rootTokens(source: string, rootTag: 'main' | 'first'): string[][] {
  const markup = markupOf(source);
  if (rootTag === 'main') {
    const classes = (tag: string): string[] =>
      [...tag.matchAll(/\bclass="([^"]*)"/g)]
        .flatMap((attr) => (attr[1] ?? '').split(/\s+/))
        .filter((token) => token !== '');
    return [...markup.matchAll(/<main\b[^>]*>/g)].map((match) => classes(match[0] ?? ''));
  }
  return topLevelClassTokens(source);
}

/** The selector text of every rule in the sheet, nested rules included. */
function sheetSelectors(): string[] {
  const sheet = readFileSync('src/assets/web.css', 'utf8');
  const dom = new JSDOM(`<style>${sheet}</style>`);
  const styles = dom.window.document.styleSheets[0];
  if (styles === undefined) throw new Error('the sheet did not parse');
  const selectors: string[] = [];
  const visit = (rules: CSSRuleList): void => {
    for (const rule of rules) {
      if ('selectorText' in rule) selectors.push((rule as CSSStyleRule).selectorText);
      const inner = 'cssRules' in rule ? (rule as CSSGroupingRule).cssRules : undefined;
      if (inner !== undefined && inner.length > 0) visit(inner);
    }
  };
  visit(styles.cssRules);
  return selectors;
}

/**
 * Whether a selector scopes rules BY `root`: some comma part's first compound
 * STARTS with `.root`. `.root { ... }` and `.root .child { ... }` do;
 * `.fleet-where .root` is a mention, and `.wrap:not(.root)` / `.wrap:has(.root)`
 * are a different subject that merely names the root.
 */
function stylesRoot(selectors: string[], root: string): boolean {
  const anchor = `.${root}`;
  return selectors.some((selector) =>
    selector.split(',').some((part) => {
      const first = part.trim().split(/[\s>+~]/)[0] ?? '';
      if (!first.startsWith(anchor)) return false;
      return !/^[A-Za-z0-9_-]/.test(first.slice(anchor.length));
    }),
  );
}

describe("the sheet's page roots", () => {
  it('fails a bare page root at the definition', () => {
    expect(
      namingProblems([
        { page: 'files', components: ['src/files/Files.svelte'], root: 'files', rootTag: 'first' },
      ]),
      'the incident name cannot be a page root',
    ).toEqual(['.files is not namespaced to the files page; a root is .files-page']);
  });

  it('reads a root selector, not a mention or another subject naming the root', () => {
    expect(stylesRoot(['.files-page { color: red }'], 'files-page'), 'the root itself').toBe(true);
    expect(
      stylesRoot(['.files-page .child { color: red }'], 'files-page'),
      'a selector scoped by the root',
    ).toBe(true);
    expect(
      stylesRoot(['.fleet-where .files { color: red }'], 'files'),
      'a descendant use is a mention, not a root',
    ).toBe(false);
    expect(
      stylesRoot(['.wrap:not(.files) { color: red }'], 'files'),
      'a pseudo-class naming the root is not its subject',
    ).toBe(false);
    expect(
      stylesRoot(['.wrap:has(.files) { color: red }'], 'files'),
      'a relational pseudo is not the root subject',
    ).toBe(false);
  });

  it('catches a second top-level classed element behind an {#if}', () => {
    const markup = '<div class="files-page"></div>{#if on}<div class="sneaky-root"></div>{/if}';
    expect(topLevelClassTokens(markup)).toHaveLength(2);
  });

  it('reads both quote styles in the route union, camelCase included', () => {
    const source =
      'export type Route =\n  | { name: \'home\' }\n  | { name: "models" }\n' +
      "  | { name: 'notFound' };\n\nexport function parseRoute() {}\n";
    expect([...namesInUnion(source)].sort()).toEqual(['home', 'models', 'notFound']);
  });

  it('names every page root to its page, and the pages cover the routes', () => {
    const routes = routeNames();
    expect(routes.size, 'every route in the union is read').toBeGreaterThanOrEqual(11);
    expect(routes.has('notFound'), 'a camelCase member is read').toBe(true);

    // Coverage: every route is either a page here or one whose root is shared
    // layout, so a new page cannot slip past the guard unnamed.
    for (const route of routes) {
      if (SHARED_ROOT_ROUTES.has(route)) continue;
      expect(
        PAGES.some((page) => page.page === route),
        `route '${route}' names its page root`,
      ).toBe(true);
    }
    for (const page of PAGES) {
      expect(routes.has(page.page), `.${page.root}'s page is still a route`).toBe(true);
    }

    const selectors = sheetSelectors();
    expect(selectors.length, 'the sheet was parsed').toBeGreaterThan(100);

    for (const page of PAGES) {
      for (const component of page.components) {
        const branches = rootTokens(readFileSync(component, 'utf8'), page.rootTag);
        if (page.rootTag === 'first') {
          expect(branches.length, `${component} draws one top-level classed element`).toBe(1);
        } else {
          expect(branches.length, `${component} has a root`).toBeGreaterThan(0);
        }
        for (const branch of branches) {
          expect(branch, `every root branch of ${component} carries .${page.root}`).toContain(
            page.root,
          );
        }
      }
      expect(
        stylesRoot(selectors, page.root),
        `.${page.root} is a subject in a web.css selector, not only a mention`,
      ).toBe(true);
    }

    // The sentinel: the incident's own root is namespaced, drawn by its
    // component and scoped by the sheet, so an empty table cannot pass.
    const files = PAGES.find((page) => page.page === 'files');
    expect(files?.root, 'the files page root is the namespaced one').toBe('files-page');
    expect(
      rootTokens(readFileSync('src/files/Files.svelte', 'utf8'), 'first')[0],
      'the files component still draws its root',
    ).toContain('files-page');

    expect(namingProblems(PAGES), 'no page root is a bare name').toEqual([]);
  });
});
