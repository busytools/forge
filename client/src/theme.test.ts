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

/** Every rule body the sheet writes for this exact selector, whitespace-normalised. */
function rulesFor(selector: string): string[] {
  const escaped = selector.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  return [...sheet.matchAll(new RegExp(`^\\s*${escaped}\\s*\\{([^}]*)\\}`, 'gm'))].map((match) =>
    (match[1] ?? '').replace(/\s+/g, ' ').trim(),
  );
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
      // The window's own edges, declared at root with an `env()` fallback and
      // written by the Android shell where the platform reports none.
      '--safe-top',
      '--safe-right',
      '--safe-bottom',
      '--safe-left',
      '--d',
      // The band's open clock, declared on the load rule rather than at root.
      '--arrive',
    ];
    // The stacks arrive beside the palette rather than inside it, from
    // `[client] font` through `fontStack`.
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

describe('the system bars', () => {
  /**
   * **The window's own bands, spent where the page draws.** Android's window
   * is edge-to-edge and a tap in the status bar's band belongs to the system,
   * so every page root - and the overlay each of them opens - pads itself by
   * the four lengths the shell writes onto the root element. A `calc` that
   * loses one of them, while a padding is being edited for any other reason,
   * is invisible on the desktop and a dead control on the phone.
   */
  it('pads every page root and the pane by the bands the shell measures', () => {
    for (const [what, selector, bands] of [
      ['the session page', '.app', ['top', 'bottom']],
      ['the projects rail', '.app .rail.left', ['top', 'bottom']],
      ['the board drawer', '.dr', ['top', 'bottom']],
      ['the palette panel', '.pal .panel', ['top']],
    ] as const) {
      // Every rule for the selector, as one: a box's padding may be spelled
      // in a rule of its own, and the later ones here carry other properties.
      const rules = rulesFor(selector);
      expect(rules.length, `${what} spells no rule at all`).toBeGreaterThan(0);
      for (const band of bands) {
        expect(rules.join(' '), `${what} drops the ${band} band`).toContain(`var(--safe-${band})`);
      }
    }
    // Every tier, not one: the phone's own `.wrap` rule overrides the padding
    // below 760px, and a tier left without the terms is the page under the bar.
    const wraps = rulesFor('.wrap');
    expect(wraps.length, 'the sheet spells .wrap at more than one width').toBeGreaterThan(1);
    for (const rule of wraps) {
      expect(rule, 'a .wrap tier drops the top band').toContain('var(--safe-top)');
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
