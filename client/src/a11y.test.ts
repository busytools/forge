import axe from 'axe-core';
import { JSDOM } from 'jsdom';
import { render } from 'svelte/server';
import { describe, expect, it } from 'vitest';

import Composer from './composer/Composer.svelte';
import type { ComposerProps, ComposerRecord } from './composer/view';
import { permissionAsk, questionAsk, record, seatRead, SLOT, wire } from './composer/testing';
import Connect from './connect/Connect.svelte';
import { homeWire } from './dev/fixture.data';
import Home from './home/Home.svelte';
import { DEFAULT_SETTINGS } from './wire/types';

type AxeWindow = Window & typeof globalThis & { axe: typeof axe };

/**
 * What axe finds wrong with a rendered page.
 *
 * **jsdom performs no layout**, so `color-contrast` comes back INCOMPLETE
 * rather than passing or failing. That is why contrast stays a rule in the
 * standard and this function only ever reports violations: a clean result
 * here is not a claim about contrast.
 */
export async function violationsOf(html: string): Promise<axe.Result[]> {
  // The shell carries what `index.html` carries: a language and a title.
  // Without the title axe reports `document-title` on every page, which is a
  // finding about this harness rather than about the page.
  //
  // `runScripts` is required for `window.eval` to run inside the document
  // rather than in the outer context, which is what lets axe see the DOM.
  const dom = new JSDOM(
    `<!doctype html><html lang="en"><head><title>forge</title></head><body>${html}</body></html>`,
    { runScripts: 'dangerously' },
  );
  // One cast, and this is its reason: the object is jsdom's window, whose
  // type cannot carry the `axe` global that the eval below injects into it.
  const window = dom.window as unknown as AxeWindow;
  window.eval(axe.source);
  const results = await window.axe.run(window.document);
  return results.violations;
}

/** Every violation id, which is what a failure should name. */
const idsOf = (html: string) => violationsOf(html).then((found) => found.map((v) => v.id));

/**
 * A button with no accessible name, so the instrument has something to find.
 *
 * **This is the file's positive control.** An empty violations array from a
 * runner that never ran reads exactly like a clean page, and every other test
 * here asserts emptiness.
 */
function PlantedViolation() {
  return '<button></button>';
}

describe('axe over the rendered pages', () => {
  it('catches a violation when one is planted', async () => {
    const html = PlantedViolation();
    expect(await idsOf(html)).toContain('button-name');
  });

  it('draws the home with no violations', async () => {
    const html = render(Home, { props: { wire: homeWire, address: '127.0.0.1:8790' } }).body;
    expect(await idsOf(html)).toEqual([]);
  });

  it('draws the connect screen with no violations', async () => {
    const html = render(Connect, {
      props: { settings: DEFAULT_SETTINGS, onconnect: () => {} },
    }).body;
    expect(await idsOf(html)).toEqual([]);
  });

  /**
   * The composer's states, because the dock's rows ARE the interaction: a
   * caret was the whole of its selection state until the row carried
   * `aria-selected`, and a caret announces nothing.
   */
  it('draws the composer with no violations', async () => {
    // Inside the landmark its page gives it: the composer is a slot in the
    // session page's own `<main>`, and rendered alone every one of its states
    // reports the page-level `region` rule instead of anything about itself.
    const draw = (held: ComposerRecord) =>
      `<main>${
        render(Composer, {
          props: {
            record: held,
            slot: SLOT,
            seat: seatRead(),
            connection: wire().connection,
          } satisfies ComposerProps,
        }).body
      }</main>`;

    expect(await idsOf(draw(record())), 'the box').toEqual([]);
    expect(await idsOf(draw(record({ pending_ask: permissionAsk() }))), 'a permission').toEqual([]);
    expect(await idsOf(draw(record({ pending_ask: questionAsk() }))), 'a question').toEqual([]);
  });
});
