// @vitest-environment jsdom
import { readFileSync } from 'node:fs';

import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

import { selectTab, surface } from '../browser/surface.svelte';
import TakeoverBar from './TakeoverBar.svelte';

const SHEET = readFileSync('src/assets/web.css', 'utf8');

vi.mock('../browser/surface.svelte', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../browser/surface.svelte')>();
  return { ...actual, selectTab: vi.fn(() => Promise.resolve(null)) };
});

let shown: ReturnType<typeof mount> | null = null;

/**
 * Take the mounted bar down and wait for it: a mount that starts before the
 * previous component finished unmounting is how two bars' handlers end up on
 * one click.
 */
async function stop(): Promise<void> {
  if (shown !== null) await unmount(shown);
  shown = null;
  document.body.innerHTML = '';
  surface.viewer = null;
  surface.controller = null;
  surface.tabs = null;
}

/** One bar, mounted for one test; never re-mounted inside a test. The
 *  surface holds the controller role, so the arrows are live unless a test
 *  takes the role away. */
async function bar(
  nav: { can_back: boolean; can_forward: boolean },
  onnav: (direction: 'back' | 'forward') => void,
  holding = true,
): Promise<HTMLElement> {
  await stop();
  surface.viewer = 1;
  surface.controller = holding ? 1 : null;
  const target = document.createElement('div');
  document.body.appendChild(target);
  shown = mount(TakeoverBar, {
    target,
    props: { title: 'browser hand-off', onback: () => {}, nav, onnav },
  });
  flushSync();
  return target;
}

function arrow(target: HTMLElement, name: 'back' | 'forward'): HTMLButtonElement {
  const button = target.querySelector<HTMLButtonElement>(`[aria-label="${name}"]`);
  if (button === null) throw new Error(`the bar draws no ${name} arrow`);
  return button;
}

afterEach(async () => {
  await stop();
});

describe("the takeover bar's history arrows", () => {
  it('draws both arrows disabled on a fresh page', async () => {
    const target = await bar({ can_back: false, can_forward: false }, () => {});
    expect(arrow(target, 'back').disabled, 'a fresh page goes nowhere back').toBe(true);
    expect(arrow(target, 'forward').disabled, 'a fresh page goes nowhere forward').toBe(true);
  });

  it('opens back after a navigation, with forward still disabled', async () => {
    const target = await bar({ can_back: true, can_forward: false }, () => {});
    expect(arrow(target, 'back').disabled, 'a link was followed: back is open').toBe(false);
    expect(arrow(target, 'forward').disabled, 'forward has nowhere to go yet').toBe(true);
  });

  it('opens forward after going back', async () => {
    const target = await bar({ can_back: true, can_forward: true }, () => {});
    expect(arrow(target, 'back').disabled, 'back after going back is still open').toBe(false);
    expect(arrow(target, 'forward').disabled, 'went back: forward is open').toBe(false);
  });

  it('sends back from the back arrow, and nothing from a disabled forward', async () => {
    const onnav = vi.fn();
    const target = await bar({ can_back: true, can_forward: false }, onnav);
    arrow(target, 'back').click();
    expect(onnav).toHaveBeenCalledWith('back');
    arrow(target, 'forward').click();
    expect(onnav, 'a disabled arrow asks for nothing').toHaveBeenCalledTimes(1);
  });

  it('sends forward from the forward arrow', async () => {
    const onnav = vi.fn();
    const target = await bar({ can_back: true, can_forward: true }, onnav);
    arrow(target, 'forward').click();
    expect(onnav).toHaveBeenCalledWith('forward');
  });

  /// **A watcher's arrows are dead even on a page with history**: the server
  /// refuses the move, and a live-looking arrow would send a gesture the
  /// person can see does nothing.
  it('draws both arrows disabled for a viewer that does not hold the role', async () => {
    const target = await bar({ can_back: true, can_forward: true }, () => {}, false);
    expect(arrow(target, 'back').disabled, 'a watcher moves nothing').toBe(true);
    expect(arrow(target, 'forward').disabled, 'and nothing forward either').toBe(true);
  });
});

describe("the takeover bar's tab picker", () => {
  /// **Each option names the tab, its page and its owner**, so the person
  /// picking knows whose agent the tab belongs to, and a switch re-attaches
  /// to that tab.
  it('lists the profile tabs with their owner and switches on a pick', async () => {
    await stop();
    surface.viewer = 1;
    surface.controller = 1;
    surface.tab = 2;
    surface.tabs = {
      profile: 'Busytools/work',
      id: 1,
      up: true,
      connected: true,
      closing_at_ms: null,
      rows: [
        {
          number: 2,
          title: 'org tabs fixture',
          url: 'http://127.0.0.1:8799/',
          owner: { org: 'Busytools', project: 'forge', label: 'lead' },
          state: 'idle',
        },
        {
          number: 3,
          title: '',
          url: 'https://example.com/',
          owner: { org: 'Busytools', project: 'forge', label: 'w1' },
          state: 'in_use',
        },
      ],
    };
    const target = document.createElement('div');
    document.body.appendChild(target);
    shown = mount(TakeoverBar, {
      target,
      props: { title: 'browser hand-off', onback: () => {} },
    });
    flushSync();

    const picker = target.querySelector<HTMLSelectElement>('.tb-pick');
    expect(picker, 'the bar draws the picker').not.toBeNull();
    const options = [...(picker?.options ?? [])].map((option) => option.text);
    expect(options[0]).toContain('2');
    expect(options[0]).toContain('org tabs fixture');
    expect(options[0]).toContain('Busytools/forge/lead');
    expect(options[1]).toContain('https://example.com/');
    expect(options[1]).toContain('Busytools/forge/w1');

    if (picker !== null) {
      picker.value = '3';
      picker.dispatchEvent(new Event('change', { bubbles: true }));
    }
    expect(selectTab).toHaveBeenCalledWith(3);
  });

  /// **The list can land after the bar.** The socket's pushed list arrives
  /// once the attach has answered, so the select is drawn after its options
  /// exist; the effect is what makes it show the tab being viewed rather
  /// than its first option. Mounting with the list already present hides
  /// that, because the option's own `selected` attribute agrees by luck.
  it('takes the viewed tab when the list arrives after the bar', async () => {
    await stop();
    surface.viewer = 1;
    surface.tab = 3;
    surface.tabs = null;
    const target = document.createElement('div');
    document.body.appendChild(target);
    shown = mount(TakeoverBar, {
      target,
      props: { title: 'browser hand-off', onback: () => {} },
    });
    flushSync();
    expect(target.querySelector('.tb-pick'), 'no list, no picker').toBeNull();

    surface.tabs = {
      profile: 'Busytools/work',
      id: 1,
      up: true,
      connected: true,
      closing_at_ms: null,
      rows: [
        {
          number: 2,
          title: 'two',
          url: 'http://b/',
          owner: { org: 'Busytools', project: 'forge', label: 'lead' },
          state: 'idle',
        },
        {
          number: 3,
          title: 'three',
          url: 'http://c/',
          owner: { org: 'Busytools', project: 'forge', label: 'lead' },
          state: 'idle',
        },
      ],
    };
    flushSync();
    const picker = target.querySelector<HTMLSelectElement>('.tb-pick');
    expect(picker, 'the bar draws the picker once the list lands').not.toBeNull();
    expect(picker?.value, 'the viewed tab is the one the picker shows').toBe('3');
  });
});

/**
 * The one rule in the sheet whose selector list names `selector`, as its
 * selector list and its body - or `null` when no rule names it.
 *
 * **Membership, not text**: a rule's selectors are reordered and its body
 * re-wrapped by any hand that touches it, and a pin on the text would go red
 * on the formatting rather than on the property.
 */
function ruleWith(selector: string): { selectors: string[]; body: string } | null {
  // Comments strip first: a rule's header otherwise carries the paragraph
  // above it, and the selector it names would never match.
  const sheet = SHEET.replace(/\/\*[\s\S]*?\*\//g, '');
  for (const match of sheet.matchAll(/([^{}]+)\{([^}]*)\}/g)) {
    const selectors = (match[1] ?? '').split(',').map((one) => one.trim());
    if (selectors.includes(selector)) return { selectors, body: match[2] ?? '' };
  }
  return null;
}

/**
 * **The feedback line's mic and send are the dock's own controls**, so their
 * rules have to be bare and keep the box they give: a rule scoped under
 * `.dock .qrow` reaches the dock's row and nothing in this bar, and a bare
 * rule that lost its 26px box falls back to UA sizing in both rows - the same
 * defect at a mouse's pointer as the 19px-wide one on a finger.
 *
 * jsdom has no layout, so what is pinned is the rule the sheet writes and the
 * markup that wears it, not a measured box; the 390x844 pointer:coarse
 * measurement is the PR's.
 */
describe("the takeover bar's feedback controls", () => {
  it("takes the dock's mic and send box at the sheet's root", () => {
    for (const control of ['micb', 'sendb']) {
      expect(
        SHEET,
        `.${control} is scoped to the dock, or lost the 26px box the dock gives it`,
      ).toMatch(new RegExp(`(^|\\n)\\.${control} \\{ flex: none; width: 26px; height: 26px;`));
      expect(SHEET, `.${control} loses its width floor on a finger`).toMatch(
        new RegExp(
          `@media \\(pointer: coarse\\) \\{\\n(?: {2}[^\\n]*\\n)*? {2}\\.${control} \\{ min-height: 44px; min-width: 44px; \\}`,
        ),
      );
    }
  });

  /// **The take card replaces the field, so it has to take the field's line**,
  /// and so must its graph: outside the line rules the card sizes to its own
  /// content (344px of a 1576px row at 1600, and 344 of 406 at 430 with a
  /// coarse pointer, measured on the dev stack), and the progress bar and the
  /// tick row keep the pill's 116px and their own width, which is the same
  /// empty-row defect one phase later. Each selector is pinned by membership
  /// so a rewrap or a reorder does not read as a lost property.
  it('gives the take card and its graph the line the feedback field gives up', () => {
    for (const selector of ['.takeline .tc', '.dock .qrow .tc', '.tb-feedback .tc']) {
      const rule = ruleWith(selector);
      expect(rule, `${selector} is in no rule, so the card sizes to its content`).not.toBeNull();
      expect(rule?.body, `${selector} is not what makes the card grow`).toMatch(/flex: 1 1 auto/);
      expect(rule?.body, `${selector} does not let it shrink below its content`).toMatch(
        /min-width: 0/,
      );
    }

    for (const selector of [
      '.takeline .tc .bar',
      '.dock .qrow .tc .bar',
      '.tb-feedback .tc .bar',
    ]) {
      const rule = ruleWith(selector);
      expect(rule, `${selector} is in no rule, so the progress bar stays a bead`).not.toBeNull();
      expect(rule?.body, `${selector} does not take the row the card took`).toMatch(
        /flex: 1 1 auto/,
      );
      expect(rule?.body, `${selector} keeps the pill's fixed width`).toMatch(/width: auto/);
      expect(rule?.body, `${selector} cannot shrink below its content`).toMatch(/min-width: 0/);
    }

    for (const selector of [
      '.takeline .tc .ticks',
      '.dock .qrow .tc .ticks',
      '.tb-feedback .tc .ticks',
    ]) {
      const rule = ruleWith(selector);
      expect(
        rule,
        `${selector} is in no rule, so the ticks stay bunched at the left`,
      ).not.toBeNull();
      expect(rule?.body, `${selector} does not take the row the card took`).toMatch(
        /flex: 1 1 auto/,
      );
      expect(rule?.body, `${selector} does not spread across it`).toMatch(
        /justify-content: space-between/,
      );
    }
  });

  /// **What a press on that row is doing is said there**, so the line's three
  /// looks have to exist: a rule that lost its box or its tone leaves the
  /// reason drawn as bare text in the bar.
  it('styles the line the bar says a take by', () => {
    const base = ruleWith('.tb-line');
    expect(base, 'no .tb-line rule, so the line draws as bare text').not.toBeNull();
    expect(base?.body, 'the line lost the band the feedback row wears').toMatch(
      /border-top: 1px solid var\(--line\)/,
    );
    expect(base?.body, 'the line lost its padding').toMatch(/padding: 7px 12px/);
    expect(ruleWith('.tb-line.bad')?.body, 'a refusal lost its tone').toMatch(
      /color: var\(--bad\)/,
    );
    expect(ruleWith('.tb-line.opening')?.body, 'an open lost its tone').toMatch(
      /color: var\(--muted\)/,
    );
  });

  /// **The rules reach the bar only through the class the markup wears**, and
  /// dropping one passes every rule pin above: the sheet would style a class
  /// no control carries.
  it('draws both controls in the feedback line, under the shared classes', async () => {
    await stop();
    surface.viewer = 1;
    surface.controller = 1;
    const target = document.createElement('div');
    document.body.appendChild(target);
    shown = mount(TakeoverBar, {
      target,
      props: {
        title: 'browser hand-off',
        onback: () => {},
        ondone: () => {},
        onnotnow: () => {},
        onfeedback: () => {},
        dictation: true,
        notes: 'the upload form rejected the file',
      },
    });
    flushSync();
    const wrong = [...target.querySelectorAll('button')].find((button) =>
      button.textContent.includes('wrong'),
    );
    if (wrong === undefined) throw new Error('the bar draws no feedback door');
    wrong.click();
    flushSync();
    expect(
      target.querySelector('.tb-feedback .micb'),
      'the feedback line lost the mic, so the sheet styles nothing here',
    ).not.toBeNull();
    expect(
      target.querySelector('.tb-feedback .sendb'),
      'the feedback line lost the send, so the sheet styles nothing here',
    ).not.toBeNull();
  });
});
