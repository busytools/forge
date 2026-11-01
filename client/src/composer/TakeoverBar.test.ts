// @vitest-environment jsdom
import { readFileSync } from 'node:fs';

import { flushSync, mount, unmount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { focusBar, focusPage, pageHoldsKeyboard } from '../browser/link';
import { selectTab, surface } from '../browser/surface.svelte';
import TakeoverBar from './TakeoverBar.svelte';

const SHEET = readFileSync('src/assets/web.css', 'utf8');

vi.mock('../browser/surface.svelte', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../browser/surface.svelte')>();
  return { ...actual, selectTab: vi.fn(() => Promise.resolve(null)) };
});

/** The two doors a native page adds: whether it holds the keyboard, and the
 *  ask that gives it back. A page with no shell has neither, which is the
 *  default here. */
vi.mock('../browser/link', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../browser/link')>();
  return {
    ...actual,
    pageHoldsKeyboard: vi.fn(() => false),
    focusPage: vi.fn(() => Promise.resolve()),
    focusBar: vi.fn(() => Promise.resolve()),
  };
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
  surface.phase = 'closed';
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
 * The first rule in the sheet whose selector list names `selector`, as its
 * selector list and its body - or `null` when no rule names it.
 *
 * **First, because one selector can appear in more than one rule** -
 * `.takeline .tc` is in the line rule and again in its min-height rule - and a
 * caller wants the first rule that names it, which is the one carrying the
 * property it pins.
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

  /// **The abandon halo stops at the card's edge on every row that puts a
  /// control beside it.** Centred, its 22px half reached through the row's gap
  /// and over the mic, and being positioned it painted and hit-tested above
  /// that button - a tap on the mic's left edge could throw the take away.
  it('keeps the abandon halo out of the control beside it', () => {
    for (const selector of [
      '.takeline .tc .x::after',
      '.dock .qrow .tc .x::after',
      '.tb-feedback .tc .x::after',
    ]) {
      const rule = ruleWith(selector);
      expect(rule, `${selector} is in no rule, so its halo is still centred`).not.toBeNull();
      expect(rule?.body, `${selector} is still anchored from the left`).toMatch(/left: auto/);
      expect(rule?.body, `${selector} does not stop at the card's edge`).toMatch(/right: 0/);
    }
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

  /// **The line names its own box.** The caret table routes by this name, so
  /// a line wearing the dock's is indistinguishable from the dock's box in
  /// every rule that reads it.
  it('names the feedback line as the bar, not the dock', async () => {
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
      },
    });
    flushSync();
    const wrong = [...target.querySelectorAll('button')].find((button) =>
      button.textContent.includes('wrong'),
    );
    if (wrong === undefined) throw new Error('the bar draws no feedback door');
    wrong.click();
    flushSync();
    const line = target.querySelector('.tb-feedback textarea');
    if (!(line instanceof HTMLTextAreaElement)) throw new Error('the bar drew no feedback line');
    expect(line.dataset['editor'], 'the line is its own box in the caret table').toBe('bar');
  });
});

/**
 * **The bar is the only thing on screen while it is up**, so it owns the
 * keyboard: it takes focus when it opens, Tab cannot walk out of it into the
 * chat the native layer is covering, and closing it gives focus back.
 */
describe("the takeover bar's keyboard", () => {
  /** A bar with every action a parked hand-off draws, so Tab has more than one stop. */
  async function full(): Promise<HTMLElement> {
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
      },
    });
    flushSync();
    return target;
  }

  function pressTab(element: Element, shift = false): void {
    element.dispatchEvent(
      new KeyboardEvent('keydown', {
        key: 'Tab',
        shiftKey: shift,
        bubbles: true,
        cancelable: true,
      }),
    );
    flushSync();
  }

  function controls(target: HTMLElement): HTMLButtonElement[] {
    const all = [...target.querySelectorAll<HTMLButtonElement>('.takeover-bar button')];
    return all.filter((button) => !button.disabled);
  }

  it('takes the keyboard into the bar when it opens', async () => {
    const target = await full();
    expect(
      document.activeElement,
      'the bar holds focus, so a key cannot land in the chat behind the page',
    ).toBe(target.querySelector('.takeover-bar'));
  });

  it('wraps Tab at both ends, so the keyboard cannot leave the bar', async () => {
    const target = await full();
    const stops = controls(target);
    expect(stops.length, 'the bar draws more than one stop').toBeGreaterThan(1);
    const first = stops[0];
    const last = stops[stops.length - 1];
    if (first === undefined || last === undefined) throw new Error('the bar lost its controls');

    last.focus();
    pressTab(last);
    expect(document.activeElement, 'Tab from the last stop wraps to the first').toBe(first);

    first.focus();
    pressTab(first, true);
    expect(document.activeElement, 'Shift+Tab from the first wraps to the last').toBe(last);
  });

  it('gives the keyboard back to whatever had it when the bar closes', async () => {
    const behind = document.createElement('button');
    behind.textContent = 'the chat';
    document.body.appendChild(behind);
    behind.focus();
    const target = document.createElement('div');
    document.body.appendChild(target);
    const mounted = mount(TakeoverBar, {
      target,
      props: { title: 'browser hand-off', onback: () => {} },
    });
    flushSync();
    expect(document.activeElement, 'the bar took the keyboard on open').not.toBe(behind);
    await unmount(mounted);
    expect(document.activeElement, 'and hands it back on close').toBe(behind);
    document.body.innerHTML = '';
  });

  /**
   * **The feedback field stops its own keys**, so the bar has to trap them
   * before that: the field is the last stop, and a Tab from it used to leave
   * the takeover (#2571).
   */
  it('wraps Tab from the feedback field, whose keys stop inside it', async () => {
    const target = await full();
    const wrong = [...target.querySelectorAll('button')].find((button) =>
      button.textContent.includes('wrong'),
    );
    if (wrong === undefined) throw new Error('the bar draws no feedback door');
    wrong.click();
    flushSync();
    const field = target.querySelector('.tb-feedback textarea');
    if (!(field instanceof HTMLTextAreaElement)) throw new Error('the bar drew no feedback field');
    // **The field's own handler ends the event for its ancestors**, so the
    // bar only sees a Tab pressed there from the capture phase.
    field.addEventListener('keydown', (event) => event.stopPropagation());
    field.focus();
    pressTab(field);
    expect(
      document.activeElement,
      'Tab from the last stop wraps to the first, not into the chat',
    ).toBe(controls(target)[0]);
  });

  /// **Shift+Tab from the bar's own focus is the other end**: the browser
  /// would walk backwards out of the bar, so the trap takes it to the last stop.
  it('wraps Shift+Tab from the bar itself to its last control', async () => {
    const target = await full();
    const bar = target.querySelector('.takeover-bar');
    if (!(bar instanceof HTMLElement)) throw new Error('the bar was not drawn');
    const stops = controls(target);
    bar.focus();
    pressTab(bar, true);
    expect(document.activeElement, 'Shift+Tab from the bar wraps to its last stop').toBe(
      stops[stops.length - 1],
    );
  });

  /// **Focus the bar no longer holds is not its to move**: a person who put
  /// the keyboard somewhere else while the takeover was up keeps it there.
  it('gives the keyboard back only when the bar still holds it', async () => {
    const before = document.createElement('button');
    before.textContent = 'before';
    const elsewhere = document.createElement('button');
    elsewhere.textContent = 'elsewhere';
    const page = document.createElement('div');
    page.append(before, elsewhere);
    document.body.append(page);
    before.focus();
    const target = document.createElement('div');
    document.body.append(target);
    const mounted = mount(TakeoverBar, {
      target,
      props: { title: 'browser hand-off', onback: () => {} },
    });
    flushSync();
    elsewhere.focus();
    await unmount(mounted);
    expect(document.activeElement, 'a focus the bar let go is not taken back').toBe(elsewhere);
    document.body.innerHTML = '';
  });

  /**
   * **Everything behind the bar is inert, and the bar is inside that page**
   * (#2571): the walk gates each sibling on the bar's own path, so the chat,
   * rail and composer stop being tabbable while the picture covers them, and
   * the attributes are given back when it lowers.
   */
  it('gates the page behind the bar inert, and gives it back on close', async () => {
    const page = document.createElement('div');
    const rail = document.createElement('div');
    const picture = document.createElement('div');
    picture.className = 'takeover';
    const host = document.createElement('div');
    page.append(rail, picture, host);
    document.body.append(page);
    const mounted = mount(TakeoverBar, {
      target: host,
      props: { title: 'browser hand-off', onback: () => {} },
    });
    flushSync();
    expect(rail.hasAttribute('inert'), 'the page behind is inert').toBe(true);
    expect(
      picture.hasAttribute('inert'),
      'the picture is kept live: its canvas and keyboard field are in there',
    ).toBe(false);
    expect(page.hasAttribute('inert'), "the bar's own path stays live").toBe(false);
    await unmount(mounted);
    expect(rail.hasAttribute('inert'), 'and the page behind is given back').toBe(false);
    document.body.innerHTML = '';
  });
});

/** **An ask with no words draws no row**: a blank reason would otherwise be an
 *  empty bordered strip that says nothing and takes the page's room. */
describe("the bar's ask", () => {
  async function drawn(reason: string | null): Promise<HTMLElement> {
    if (shown !== null) await unmount(shown);
    document.body.innerHTML = '';
    const target = document.createElement('div');
    document.body.appendChild(target);
    shown = mount(TakeoverBar, {
      target,
      props: { title: 'browser hand-off', reason, onback: () => {} },
    });
    flushSync();
    return target;
  }

  it('draws the ask only when it has words', async () => {
    expect((await drawn(null)).querySelector('.tb-ask'), 'no reason, no row').toBeNull();
    expect(
      (await drawn('   ')).querySelector('.tb-ask'),
      'a blank reason draws no row either',
    ).toBeNull();
    expect(
      (await drawn('The sign-in page is showing a CAPTCHA.')).querySelector('.tb-ask')?.textContent,
      'and words are drawn as they came',
    ).toBe('The sign-in page is showing a CAPTCHA.');
  });
});

/**
 * **The keyboard follows the picture.** On a desktop shell a native layer over
 * this page holds AppKit first responder while the takeover is up, so the
 * bar's DOM focus is not where keys land: the shell hands the keyboard over
 * on the pane chords, and the bar gives it back to the page rather than
 * keeping a ring on a control it cannot reach (#2571).
 */
describe("the takeover bar's keyboard over a native page", () => {
  const holds = vi.mocked(pageHoldsKeyboard);
  const ask = vi.mocked(focusPage);
  const askBar = vi.mocked(focusBar);

  /** One bar with the native page's claim on, mounted for one test. */
  async function nativeBar(phase: 'live' | 'failed'): Promise<HTMLElement> {
    await stop();
    surface.phase = phase;
    surface.viewer = 1;
    surface.controller = 1;
    const target = document.createElement('div');
    document.body.appendChild(target);
    shown = mount(TakeoverBar, {
      target,
      props: { title: 'browser hand-off', onback: () => {}, ondone: () => {}, onnotnow: () => {} },
    });
    flushSync();
    return target;
  }

  function paneChord(element: Element, key: 'ArrowUp' | 'ArrowDown'): KeyboardEvent {
    const chord = new KeyboardEvent('keydown', {
      key,
      metaKey: true,
      altKey: true,
      bubbles: true,
      cancelable: true,
    });
    element.dispatchEvent(chord);
    flushSync();
    return chord;
  }

  beforeEach(() => {
    holds.mockReturnValue(true);
    ask.mockReset();
    ask.mockReturnValue(Promise.resolve());
    askBar.mockReset();
    askBar.mockReturnValue(Promise.resolve());
  });

  afterEach(() => {
    holds.mockReturnValue(false);
  });

  it('leaves the keyboard on the page rather than taking it on open', async () => {
    const target = await nativeBar('live');
    expect(
      document.activeElement,
      'the native page holds the keys, so the bar claiming the DOM focus would be a lie',
    ).not.toBe(target.querySelector('.takeover-bar'));
  });

  it('takes the keyboard when the shell hands it over', async () => {
    const target = await nativeBar('live');
    window.dispatchEvent(new Event('forge-bar-focus'));
    flushSync();
    expect(
      document.activeElement,
      'the chord put the keyboard in the webview, so the bar takes it',
    ).toBe(target.querySelector('.takeover-bar'));
  });

  it('hands the keyboard back to the page on the pane chord, and lets the focus go', async () => {
    const target = await nativeBar('live');
    const button = target.querySelector<HTMLButtonElement>('.tb-btn');
    if (button === null) throw new Error('the bar drew no control');
    button.focus();
    const chord = paneChord(button, 'ArrowUp');
    expect(ask, 'the shell is asked for the native page').toHaveBeenCalledTimes(1);
    expect(
      document.activeElement,
      'the DOM gives the focus up with the keys, rather than ringing a control they cannot reach',
    ).not.toBe(button);
    expect(chord.defaultPrevented, 'the chord is the bar panes, not the page').toBe(true);
  });

  it('gives the focus up when the picture takes the keyboard back', async () => {
    const target = await nativeBar('live');
    const button = target.querySelector<HTMLButtonElement>('.tb-btn');
    if (button === null) throw new Error('the bar drew no control');
    button.focus();
    window.dispatchEvent(new Event('forge-page-focus'));
    flushSync();
    expect(document.activeElement, 'the ring goes with the keyboard').not.toBe(button);
  });

  /// **A focus inside the bar is the keyboard's too**: a take's landing puts
  /// the caret in the feedback field while the picture still holds AppKit's
  /// first responder, so the shell has to hand the keys over.
  it('asks the shell for the keyboard when focus lands inside it', async () => {
    const target = await nativeBar('live');
    const button = target.querySelector<HTMLButtonElement>('.tb-btn');
    if (button === null) throw new Error('the bar drew no control');
    button.focus();
    flushSync();
    expect(
      askBar,
      'the webview must take the keys for the DOM focus to mean anything',
    ).toHaveBeenCalledTimes(1);
  });

  it('asks for nothing where this document already owns the keyboard', async () => {
    holds.mockReturnValue(false);
    const target = await nativeBar('live');
    const button = target.querySelector<HTMLButtonElement>('.tb-btn');
    if (button === null) throw new Error('the bar drew no control');
    button.focus();
    flushSync();
    expect(askBar, 'no native page holds a keyboard to take back').not.toHaveBeenCalled();
  });

  /// **A failed page has no native layer to hold anything**, so the bar is the
  /// only thing left to act on and takes the keyboard itself.
  it('takes the keyboard when the page failed, with no picture to hold it', async () => {
    const target = await nativeBar('failed');
    expect(document.activeElement, 'the bar is the surface that remains').toBe(
      target.querySelector('.takeover-bar'),
    );
  });

  /// **The failure lands under a bar that is already up**, which is the case
  /// the mount cannot cover: a live page held the keys, its picture is taken
  /// down with the failure, and the bar is what the keyboard has left.
  it('takes the keyboard when the page fails under it', async () => {
    const target = await nativeBar('live');
    expect(document.activeElement, 'the page holds the keys while it is live').not.toBe(
      target.querySelector('.takeover-bar'),
    );
    surface.phase = 'failed';
    flushSync();
    expect(document.activeElement, 'the picture is gone, so the bar takes them').toBe(
      target.querySelector('.takeover-bar'),
    );
  });

  it('keeps the keyboard when the chord has no page to hand it to', async () => {
    const target = await nativeBar('failed');
    const bar = target.querySelector('.takeover-bar');
    if (bar === null) throw new Error('the bar was not drawn');
    const chord = paneChord(bar, 'ArrowUp');
    expect(ask, 'no picture, no ask').not.toHaveBeenCalled();
    expect(document.activeElement, 'and the keyboard stays where the bar took it').toBe(bar);
    expect(chord.defaultPrevented, 'the chord is still the bar panes').toBe(true);
  });
});
