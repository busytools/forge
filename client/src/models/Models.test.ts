// @vitest-environment jsdom
import { readFileSync } from 'node:fs';

import { JSDOM } from 'jsdom';
import { flushSync, mount, tick, unmount } from 'svelte';
import { afterEach, describe, expect, it } from 'vitest';

import type { DictateModelsWire } from '../wire/models';
import Models from './Models.svelte';
import ModelsBody from './ModelsBody.svelte';
import { fakeConnection, MODELS, modelsWire } from './testing';

const drawn: ReturnType<typeof mount>[] = [];
const hosts: HTMLElement[] = [];

afterEach(() => {
  for (const component of drawn.splice(0)) void unmount(component);
  for (const host of hosts.splice(0)) host.remove();
});

/**
 * What a forge with `[dictate] enabled` unset answers: no pins, no check, and
 * no rows - the catalogue is only read while the section is on.
 */
function offWire(): DictateModelsWire {
  return {
    ...modelsWire,
    enabled: false,
    in_use: [],
    updates: [],
    check: { state: 'never' },
    rows: [],
  };
}

/** The page as it draws, mounted so a control can be reached. */
function open(wire: DictateModelsWire = modelsWire, oncheck: () => void = () => {}) {
  const host = document.createElement('div');
  document.body.append(host);
  hosts.push(host);
  const component = mount(ModelsBody, { target: host, props: { wire, oncheck } });
  drawn.push(component);
  return host;
}

describe('the models page as it draws', () => {
  /**
   * The two pins, with the facts each side is read from: the pin's own
   * declaration on the first line, the feed's measurement on the second, and
   * the live state as a chip.
   */
  it('draws the models in use', () => {
    const html = open().innerHTML;

    expect(html).toContain('cohere-transcribe-03-2026-Q4_K_M.gguf');
    expect(html).toContain('1.56 GB');
    expect(html).toContain('Q4_K_M');
    expect(html).toContain('sha 0ea56826');
    expect(html).toContain('72.9\u{d7} realtime on m4-max');
    expect(html).toContain('loaded');
    // The normalizer is in no feed and still draws: its own runtime stands
    // where the measurement would be.
    expect(html).toContain('s1-mini-f16.gguf');
    expect(html).toContain('llama.cpp');
    expect(html).toContain('not in the feed');
  });

  /**
   * The proposal's comparison, both sides of both numbers, and the link an
   * adoption takes: the note under it says a proposal becomes a pull request.
   */
  it('draws what the feed proposes', () => {
    const html = open().innerHTML;

    expect(html).toContain('update available');
    expect(html).toContain('Granite Speech 5.0 470M TurboCTC');
    expect(html).toContain('388.8\u{d7} vs 72.9\u{d7}');
    expect(html).toContain('FLEURS-en 4.61 vs 5.08');
    expect(html).toContain('a pull request');
  });

  /**
   * A check that is out is drawn as the core's own state, with no Check now
   * to press: a second check is refused, and a control that reports nothing
   * when pressed reads as broken.
   */
  it('draws a check in flight without the control to start another', () => {
    const host = open({
      ...modelsWire,
      check: { state: 'checking' },
    });

    expect(host.textContent).toContain('checking the catalogue');
    expect(host.querySelector('.status button'), 'a check in flight offers another').toBeNull();
  });

  /** The check's own click is what dispatches, once per press. */
  it('asks for a check when the control is pressed', () => {
    let checks = 0;
    const host = open(modelsWire, () => {
      checks += 1;
    });

    const button = host.querySelector('button');
    expect(button?.textContent).toContain('Check now');
    button?.click();
    flushSync();

    expect(checks).toBe(1);
  });

  /**
   * Finding a model is this side's: the whole feed arrives with the read, so
   * the box filters what is here as it is typed and asks the server for
   * nothing. Each match is a LINK to the entry's own document - a row that
   * goes nowhere is what a reader clicks first and finds nothing behind.
   */
  it('filters the feed as the box is typed in, and each row opens its entry', () => {
    const host = open();
    const input = host.querySelector('input');
    expect(input).not.toBeNull();
    // Nothing typed: no list at all.
    expect(host.textContent).toContain('type a name');

    if (input !== null) {
      input.value = 'parakeet';
      input.dispatchEvent(new Event('input', { bubbles: true }));
      flushSync();
    }

    expect(host.textContent).toContain('parakeet-unified-en-0.6b');
    expect(host.textContent, 'a row nothing matched was drawn').not.toContain(
      'granite-speech-5.0-470m-turboctc',
    );
    expect(host.textContent).toContain('English only');
    expect(host.textContent).toContain('streaming');

    const row = host.querySelector<HTMLAnchorElement>('.cand');
    expect(row?.tagName, 'a result is not a link').toBe('A');
    expect(row?.getAttribute('href')).toContain('/catalog/parakeet-unified-en-0.6b.json');
    expect(row?.getAttribute('target')).toBe('_blank');
  });

  /**
   * **The hairline between rows survives the row becoming a link.** Every
   * `.cand` is its `<li>`'s only child, so a `:last-child` written on the ROW
   * matches all of them at once and the separator dies in the whole list -
   * which is exactly what the anchor restructure did, silently. The rule
   * belongs to the li.
   *
   * **Asserted on selectors rather than computed styles**, and deliberately:
   * jsdom drops a `var()` inside a shorthand (`border-bottom: 1px solid
   * var(--line)` computes to `0px none`), so a cascade read here would be an
   * instrument that cannot see the very rule it guards. What runs instead is
   * the real selector engine over the real markup.
   */
  it('keeps a hairline between candidate rows, and none under the last', () => {
    const host = open();
    const input = host.querySelector('input');
    if (input !== null) {
      input.value = 'granite';
      input.dispatchEvent(new Event('input', { bubbles: true }));
      flushSync();
    }

    const sheet = readFileSync('src/assets/web.css', 'utf8');
    const dom = new JSDOM(`<style>${sheet}</style><div class="models">${host.innerHTML}</div>`);
    const rows = [...dom.window.document.querySelectorAll('.models .list li')];
    expect(rows.length, 'the search drew no rows to read').toBeGreaterThan(1);

    // The hazard itself, so a reader sees why the rule cannot key on the row.
    for (const li of rows) {
      expect(
        li.querySelector('.cand')?.matches('.models .cand:last-child'),
        "a row that is NOT its li's last child",
      ).toBe(true);
    }
    // And the rules that decide it: the last li drops the hairline, and no
    // `.cand` rule claims one.
    expect(rows[0]?.matches('.models .list li:last-child'), 'the first row is the last').toBe(
      false,
    );
    expect(
      rows[rows.length - 1]?.matches('.models .list li:last-child'),
      'the last row does not carry the rule that drops its hairline',
    ).toBe(true);
    expect(sheet, 'the hairline rule is back on the row, which kills it list-wide').not.toContain(
      '.models .cand:last-child',
    );
    // Read as text, because the computed value is what jsdom cannot give:
    // this is the half that says the li is where the hairline is drawn.
    expect(sheet, 'nothing draws the hairline on the li').toMatch(
      /\.models \.list li(?::last-child)? \{[^}]*border-bottom/,
    );
  });

  it('says so when nothing matches', () => {
    const host = open();
    const input = host.querySelector('input');
    if (input !== null) {
      input.value = 'wav2vec';
      input.dispatchEvent(new Event('input', { bubbles: true }));
      flushSync();
    }

    expect(host.textContent).toContain('no entry matches');
    expect(host.querySelector('.cand')).toBeNull();
  });

  /**
   * Dictation off is its own state, and the page says which key is unset
   * rather than drawing an empty list that reads as a broken page.
   *
   * **The whole snapshot of an off forge, not part of it.** Nothing starts
   * the preflight or the catalogue read while the section is off, so the
   * server answers no rows and no check: a search box here would answer every
   * query with "no entry matches", and the page has to say why instead.
   */
  it('draws the off state, which has no catalogue to search', () => {
    const host = open(offWire());

    expect(host.textContent).toContain('dictation is off');
    expect(host.textContent).toContain('[dictate] enabled');
    expect(host.textContent).toContain('not checked yet');
    // No search box, and the note says why: the feed is not read.
    expect(host.querySelector('input'), 'an off forge draws a search box').toBeNull();
    expect(host.textContent).toContain('read only with');
    // And no Check now: with `[dictate]` off the core refuses the check, so a
    // control here would report nothing when pressed.
    expect(host.querySelector('.status button'), 'an off forge draws a Check now').toBeNull();
  });

  /**
   * A check state this client does not know says so, and never draws as a
   * fresh one: `up to date` here would be a claim about a feed nothing read.
   */
  it('draws a check state it cannot read as its own state', () => {
    const host = open({ ...modelsWire, check: { state: 'unknown' } as never, updates: [] });

    expect(host.textContent).toContain('this client');
    expect(host.textContent).not.toContain('up to date');
  });

  /** A feed that did not answer carries its own reason, in the error tone. */
  it('draws a check that could not reach the catalogue', () => {
    const host = open({
      ...modelsWire,
      check: { state: 'unreachable', error: 'github.com answered 502' },
      updates: [],
    });

    expect(host.textContent).toContain('the catalogue could not be reached');
    expect(host.textContent).toContain('github.com answered 502');
  });
});

describe('the models route as it draws', () => {
  /** No connection, no read: the page says it is reading rather than drawing
   * an empty catalogue. */
  it('draws the loading line before the first read lands', () => {
    const host = document.createElement('div');
    document.body.append(host);
    hosts.push(host);
    const component = mount(Models, {
      target: host,
      props: { connection: null },
    });
    drawn.push(component);

    expect(host.textContent).toContain('Reading the models...');
    expect(host.querySelector('.models .block')).toBeNull();
  });

  /** Mount the route on a connection a test drives, over the given host. */
  function route(forge: ReturnType<typeof fakeConnection>): HTMLElement {
    const host = document.createElement('div');
    document.body.append(host);
    hosts.push(host);
    const component = mount(Models, { target: host, props: { connection: forge.connection } });
    drawn.push(component);
    return host;
  }

  /**
   * **The page's live wiring, which no string-level test can see.** The route
   * subscribes the catalogue itself, hands the snapshot to the body, and its
   * Check now is the one caller of the command - three links a rename or a
   * deleted effect breaks silently: nothing would be subscribed, the page
   * would read nothing, and the press would do nothing, with every other test
   * green.
   */
  it('subscribes the catalogue, draws what it answers, and checks on its control', async () => {
    const forge = fakeConnection();
    const host = route(forge);
    await tick();

    expect(forge.subscribed, 'the route did not subscribe the catalogue').toEqual([MODELS]);

    forge.arrive({ kind: 'snapshot', subject: MODELS, data: modelsWire });
    await tick();
    expect(host.textContent, 'the snapshot never reached the page').toContain('update available');

    const check = host.querySelector<HTMLButtonElement>('.status button');
    expect(check, 'the check line drew no control to press').not.toBeNull();
    check?.click();
    flushSync();
    expect(forge.dispatched, 'Check now dispatched nothing').toEqual(['dictate_catalogue_check']);
    expect(forge.refreshed, 'the click did not ask for the read').toEqual([MODELS]);
  });

  /**
   * **Leaving the page releases the catalogue.** The effect's teardown is the
   * only thing that does it, and the route's own doc promises it: without it
   * the unsubscribe never goes, and a forge is left encoding a read nobody
   * draws for the rest of the session.
   */
  it('releases the catalogue when the route is left', async () => {
    const forge = fakeConnection();
    route(forge);
    await tick();
    expect(forge.subscribed).toEqual([MODELS]);

    // The route this test just mounted is the last one drawn.
    const component = drawn.pop();
    if (component === undefined) throw new Error('the route was not mounted');
    void unmount(component);

    expect(forge.unsubscribed, 'leaving the page left the catalogue subscribed').toEqual([MODELS]);
    expect(forge.listening(), 'the release left a listener on the connection').toBe(0);
  });

  /**
   * A refused subscription is the server's own words on the page. Without the
   * branch, the store's refusal never turns into a drawing and the page reads
   * `Reading the models...` for the life of the connection.
   */
  it('draws the refusal in the server words', async () => {
    const forge = fakeConnection();
    const host = route(forge);
    await tick();

    forge.arrive({ kind: 'error', what: 'subscribe', why: 'no models on this forge' });
    await tick();

    expect(host.textContent).toContain('no models on this forge');
    expect(host.textContent, 'a refusal drew the loading line').not.toContain('Reading the models');
  });
});
