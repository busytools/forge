import { readFileSync, readdirSync } from 'node:fs';
import { join } from 'node:path';

import { describe, expect, it } from 'vitest';

const CLIENT = new URL('../../', import.meta.url).pathname;
const SRC = join(CLIENT, 'src');
const MODULES = join(CLIENT, 'node_modules');
const SHEET = readFileSync(join(SRC, 'assets/web.css'), 'utf8');

/**
 * **Only the weights the vendored cuts carry may be asked for.** Fira Code
 * ships static cuts, and the sheet declares the two it vendors; every
 * `font-weight` above the heaviest of them has no face behind it and the
 * browser fakes it by smearing the Medium, which is what made emphasised
 * prose read as three loud things before #1509.
 *
 * **The ceiling is read from the faces, not written down**, and the scan
 * covers every way the client can ask for a weight: the sheet, every
 * component style, the code and diff emitters' inline styles (the keyword
 * `bold` is a weight too), and the two vendor sheets the sheet imports. A
 * vendor weight above the ceiling is answered by an override in this file,
 * because the imports themselves are upstream and are not forked.
 */
function vendoredWeights(sheet: string): number[] {
  return [...sheet.matchAll(/@font-face\s*\{([^}]*)\}/g)].flatMap((block) =>
    [...(block[1] ?? '').matchAll(/font-weight:\s*(\d+)/g)].map((match) => Number(match[1])),
  );
}

/** One style rule from a stylesheet: its selector and its declarations. */
interface SheetRule {
  selector: string;
  body: string;
}

/** Every style rule of `sheet`, at any nesting depth. */
function rulesOf(sheet: string): SheetRule[] {
  const rules: SheetRule[] = [];
  type Open = { kind: 'at' } | { kind: 'rule'; rule: SheetRule };
  const stack: Open[] = [];
  let text = '';
  for (const char of sheet) {
    const current = stack[stack.length - 1];
    if (current?.kind === 'rule') {
      if (char === '}') {
        current.rule.body = text;
        stack.pop();
        text = '';
      } else text += char;
      continue;
    }
    if (char === '{') {
      const selector = text.trim();
      text = '';
      if (selector.startsWith('@')) stack.push({ kind: 'at' });
      else {
        const rule: SheetRule = { selector, body: '' };
        rules.push(rule);
        stack.push({ kind: 'rule', rule });
      }
    } else if (char === '}') {
      if (current?.kind === 'at') stack.pop();
      text = '';
    } else if (char === ';' && text.trimStart().startsWith('@')) {
      // A statement at-rule (`@import ...;`) ends here; its text must not run
      // into the selector of the next rule, or that rule parses as an at-rule.
      text = '';
    } else text += char;
  }
  return rules;
}

/** A weight as written, numeric or the keyword forms. */
function weightOf(value: string): number {
  if (value === 'bold' || value === 'bolder') return 700;
  return Number(value);
}

/** Every weight a stylesheet asks for. */
function sheetWeights(sheet: string): Array<{ selector: string; weight: number }> {
  return rulesOf(sheet).flatMap((rule) =>
    [...rule.body.matchAll(/font-weight:\s*(bold|bolder|\d+)/g)].map((match) => ({
      selector: rule.selector,
      weight: weightOf(match[1] ?? ''),
    })),
  );
}

/** Every style file a page can draw from: components, the sheet and the emitters. */
function styleFiles(directory: string): string[] {
  const found: string[] = [];
  for (const entry of readdirSync(directory, { withFileTypes: true })) {
    const path = join(directory, entry.name);
    if (entry.isDirectory()) {
      if (path === join(SRC, 'dev')) continue;
      found.push(...styleFiles(path));
      continue;
    }
    if (entry.name.endsWith('.test.ts')) continue;
    if (
      entry.name.endsWith('.svelte') ||
      entry.name.endsWith('.css') ||
      entry.name.endsWith('.ts')
    ) {
      found.push(path);
    }
  }
  return found;
}

describe('the weights the client asks of its vendored face', () => {
  it('never asks for a weight no vendored cut carries', () => {
    const faces = vendoredWeights(SHEET);
    expect(faces.length, 'the sheet declares its cuts').toBeGreaterThanOrEqual(2);
    const ceiling = Math.max(...faces);

    const files = styleFiles(SRC);
    const declarations = files.flatMap((path) =>
      [...readFileSync(path, 'utf8').matchAll(/font-weight:\s*(bold|bolder|\d+)/g)].map(
        (match) => ({
          path,
          weight: weightOf(match[1] ?? ''),
        }),
      ),
    );
    // The denominators, so a scan that stopped reading cannot pass: the sheet
    // alone carries about a hundred declarations, the components carry their
    // own, and the two emitters are the only `.ts` files that write a weight
    // inline - a scan that skipped either family fails here.
    expect(declarations.length, 'the scan read the whole client').toBeGreaterThanOrEqual(90);
    expect(
      declarations.filter((entry) => entry.path.endsWith('.svelte')).length,
      'the scan reached the component styles',
    ).toBeGreaterThanOrEqual(5);
    for (const path of ['format/diff.ts', 'format/highlight.ts']) {
      expect(
        files.some((file) => file.endsWith(path)),
        `the scan reached the inline emitter ${path}`,
      ).toBe(true);
    }

    const synthesized = declarations.filter((entry) => entry.weight > ceiling);
    expect(
      synthesized.map((entry) => `${entry.path.replace(`${SRC}/`, '')}: ${entry.weight}`),
      `weights with no face behind them (the heaviest vendored cut is ${ceiling})`,
    ).toEqual([]);

    // The imported sheets are upstream, so their weights are not edited - but
    // every one above the ceiling has to be answered by an override here.
    const imports = [...SHEET.matchAll(/@import\s+['"]([^'"]+)['"]/g)].map(
      (match) => match[1] ?? '',
    );
    expect(imports.length, 'the sheet imports the vendor sheets').toBeGreaterThanOrEqual(2);
    // Comments are stripped and attribute quotes normalised, because the
    // override that answers a vendor rule is that rule's selector written in
    // this sheet - the spelling of a quote is not a difference.
    const plain = (sheet: string): string => sheet.replace(/\/\*[\s\S]*?\*\//g, '');
    const selectorsOf = (selector: string): string[] =>
      selector
        .split(',')
        .map((one) => one.trim().replace(/'/g, '"'))
        .filter((one) => one !== '');
    const overrides = sheetWeights(plain(SHEET)).filter((entry) => entry.weight === ceiling);
    const overridden = new Set(overrides.flatMap((rule) => selectorsOf(rule.selector)));
    const uncovered: string[] = [];
    for (const spec of imports) {
      for (const weight of sheetWeights(plain(readFileSync(join(MODULES, spec), 'utf8')))) {
        if (weight.weight <= ceiling) continue;
        for (const selector of selectorsOf(weight.selector)) {
          if (!overridden.has(selector)) uncovered.push(`${spec}: ${selector} ${weight.weight}`);
        }
      }
    }
    expect(uncovered, 'vendor weights above the ceiling with no override here').toEqual([]);
  });
});
