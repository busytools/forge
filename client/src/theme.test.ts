import { readFileSync } from 'node:fs';

import { describe, expect, it } from 'vitest';

import { applySettings, fontStack, rootTokens, type StyleTarget } from './theme';
import { DEFAULT_AXES } from './session/wire';
import { FONT_NAMES, THEME_NAMES, type ClientSettings } from './wire/types';

const sheet = readFileSync(new URL('./assets/web.css', import.meta.url), 'utf8');

/**
 * Every first capture group. `noUncheckedIndexedAccess` types an indexed
 * match as `string | undefined` because it cannot know the pattern has a
 * group; the loop says both that it does and that this code does not assume
 * it.
 */
function captures(text: string, pattern: RegExp): string[] {
  const found: string[] = [];
  for (const match of text.matchAll(pattern)) {
    const group = match[1];
    if (group !== undefined) found.push(group);
  }
  return found;
}

/** A root that records what was hung on it, so no DOM is needed. */
function stubRoot() {
  const set = new Map<string, string>();
  const root: StyleTarget = {
    style: {
      setProperty: (token, value) => void set.set(token, value),
      removeProperty: (token) => void set.delete(token),
    },
  };
  return { root, set };
}

describe('the palette', () => {
  /**
   * Every token the sheet reads resolves, read from the sheet rather than
   * from a list here: a list could only ever fail on a rename inside this
   * module, which is the inverse of what the test is for.
   */
  it('resolves every token the stylesheet reads', () => {
    // The sheet declares these for itself; the palette is not their home.
    const local = [
      '--r',
      '--cols',
      '--pad',
      '--fs-prose',
      '--fs-base',
      '--fs-group',
      '--fs-data',
      '--fs-label',
      '--fs-title',
      '--ins',
      '--rail-l',
      '--rail-r',
    ];
    // The stacks arrive beside the palette rather than inside it, from
    // `[web] font` through `fontStack`.
    const stacks = new Set(['--ui', '--mono']);
    // The retired highlighter's, read by the sheet's `.k` / `.s` / `.f` /
    // `.dif` rules and deliberately NOT carried: colouring code and a
    // working tree is the client's work from a maintained module, and those
    // rules go with it. Named rather than pattern-matched, so a token the
    // sheet starts reading that is NOT one of these is still a failure.
    const retired = new Set(['--syn-key', '--syn-str', '--syn-fn', '--add-bg', '--del-bg']);

    const reads = new Set(
      captures(sheet, /var\((--[a-z0-9-]+)/g).filter(
        (token) => !local.includes(token) && !stacks.has(token) && !retired.has(token),
      ),
    );
    expect(reads.size, 'the sheet reads the palette somewhere').toBeGreaterThan(0);

    const tokens = rootTokens(null);
    for (const token of reads) {
      expect(tokens, `the palette must resolve ${token}, which the sheet reads`).toHaveProperty(
        token,
      );
    }
  });

  /**
   * The syntax and diff tokens colour code and a working tree, which the
   * server does not own. Carried here they would pin the client to a
   * renderer that is gone.
   */
  it('does not carry the retired highlighter tokens', () => {
    const tokens = rootTokens(THEME_NAMES[0]);
    for (const token of ['--syn-key', '--syn-str', '--syn-fn', '--add-bg', '--del-bg']) {
      expect(tokens, `${token} belongs to the server's renderer`).not.toHaveProperty(token);
    }
  });
});

describe('the type scale', () => {
  /** The prose step is the chat's reading size, and it is the scale's base
   * step rather than one above it: a row is data and a label sits on data,
   * but prose is read, so nothing in the chat is larger than it. */
  it("draws the chat's prose at the base step", () => {
    const declared = captures(sheet, /--fs-prose:\s*([^;]+);/g);

    expect(declared, 'the sheet declares the prose step once').toHaveLength(1);
    expect(declared[0], 'the prose is the base step, not a step above it').toBe('15.5px');
  });
});

describe('the typefaces', () => {
  /** By value: two distinct objects satisfy any "not equal" shape. */
  it('draws each name as its own stack', () => {
    expect(fontStack(null)).toEqual({
      ui: '"Fira Code",system-ui,-apple-system,"Segoe UI",sans-serif',
      mono: '"Fira Code",ui-monospace,Menlo,monospace',
    });
    expect(fontStack(FONT_NAMES[0])).toEqual({
      ui: 'system-ui,-apple-system,"Segoe UI",sans-serif',
      mono: 'ui-monospace,Menlo,monospace',
    });
  });

  it('draws no stack for a name outside the shipped set', () => {
    expect(FONT_NAMES).not.toContain('comic');
    expect(fontStack('comic')).toBeNull();
  });

  /** The stack asks for the families the sheet declares faces for. */
  it('asks for the faces the sheet ships', () => {
    const declared = captures(sheet, /@font-face\s*{[^}]*font-family:\s*"([^"]+)"/g);
    expect(declared).toHaveLength(2);
    const builtIn = fontStack(null);
    for (const face of declared) {
      const asked =
        (builtIn?.ui ?? '').includes(`"${face}"`) || (builtIn?.mono ?? '').includes(`"${face}"`);
      expect(asked, `${face} is shipped and never asked for`).toBe(true);
    }
  });
});

describe('applying what the greeting carried', () => {
  const settings = (over: Partial<ClientSettings> = {}): ClientSettings => ({
    mark: null,
    theme: null,
    font: null,
    dictate: DEFAULT_AXES,
    ...over,
  });

  it('hangs the palette and the stacks on the root', () => {
    const { root, set } = stubRoot();
    applySettings(settings(), root);
    expect(set.size).toBe(Object.keys(rootTokens(null)).length + 2);
    expect(set.get('--bg')).toBe(rootTokens(null)['--bg']);
    expect(set.get('--ui')).toBe(fontStack(null)?.ui);
    expect(set.get('--mono')).toBe(fontStack(null)?.mono);
  });

  it('hangs the stacks under the name the server sent', () => {
    const { root, set } = stubRoot();
    applySettings(settings({ font: 'system' }), root);
    expect(set.get('--ui')).toBe(fontStack('system')?.ui);
    expect(set.get('--mono')).toBe(fontStack('system')?.mono);
  });

  /**
   * A name outside the shipped set contributes nothing, so a stack an
   * earlier greeting hung has to go rather than linger beside a palette that
   * no longer matches it.
   */
  it('takes back a stack when the name resolves to none', () => {
    const { root, set } = stubRoot();
    applySettings(settings({ font: 'system' }), root);
    expect(set.has('--ui')).toBe(true);
    applySettings(settings({ font: 'comic' }), root);
    expect(set.has('--ui'), 'a refused name left the previous stack behind').toBe(false);
    expect(set.has('--mono')).toBe(false);
  });
});
