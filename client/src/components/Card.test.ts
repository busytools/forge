// @vitest-environment jsdom
import { readFileSync } from 'node:fs';

import { JSDOM } from 'jsdom';
import { mount, unmount } from 'svelte';
import { afterEach, describe, expect, it } from 'vitest';

import type { BandCard } from '../home/view';
import ModelsBody from '../models/ModelsBody.svelte';
import { modelsWire } from '../models/testing';
import Card from './Card.svelte';

const drawn: ReturnType<typeof mount>[] = [];
const hosts: HTMLElement[] = [];

afterEach(() => {
  for (const component of drawn.splice(0)) void unmount(component);
  for (const host of hosts.splice(0)) host.remove();
});

/** One band card, mounted so the element itself can be reached. */
function box(over: Partial<BandCard> = {}): HTMLElement {
  const host = document.createElement('div');
  document.body.append(host);
  hosts.push(host);
  drawn.push(
    mount(Card, {
      target: host,
      props: {
        title: 'dictation',
        tone: 'ready',
        value: '2 of 2 loaded',
        detail: 'loaded, loaded',
        href: '/models',
        ...over,
      },
    }),
  );
  const [card] = host.children;
  if (card === undefined) throw new Error('the card drew nothing');
  return card as HTMLElement;
}

describe('the band card', () => {
  /**
   * **The card that opens something is an ANCHOR, not a box carrying an
   * `href`.** A `div` with the attribute draws identically, and this file's
   * own door test would pass on it - the string `href="/models"` is in the
   * markup either way - while nothing about it is a link: no keyboard focus,
   * no middle-click, no open-in-a-new-tab, and the chevron promises a way in
   * no input can take.
   */
  it('is an anchor when it opens something, and a plain box when it does not', () => {
    const door = box();

    expect(door.tagName, 'the door is not a link element').toBe('A');
    expect(door.getAttribute('href')).toBe('/models');
    expect(door.tabIndex, 'a link is reachable by keyboard').toBe(0);
    expect(door.querySelector('.arw'), 'the door draws no marker').not.toBeNull();

    const fact = box({ href: null });

    expect(fact.tagName, 'a card that opens nothing is not a link').toBe('DIV');
    expect(fact.querySelector('.arw'), 'a card that opens nothing draws a door marker').toBeNull();
  });

  /**
   * **The band's marks stay flat discs, and the page's keep the triangle.**
   * The shared mark block gained `.dot.warn`'s clip-path for the models page,
   * and the band's reset has to clear it: without that line every warn card in
   * the home - the gateway binding, dictation loading, accounts probing - grows
   * a shape the band never drew, which is a models-scoped change showing on a
   * sibling surface. The cascade is read from the shipped sheet, because the
   * collision is between two selectors, not inside either component.
   */
  it('keeps the band flat while the models page keeps its triangle', () => {
    const band = document.createElement('div');
    band.className = 'band';
    document.body.append(band);
    hosts.push(band);
    drawn.push(
      mount(Card, {
        target: band,
        props: { title: 'gateway', tone: 'warn', value: 'binding', detail: 'listener', href: null },
      }),
    );

    const page = document.createElement('div');
    document.body.append(page);
    hosts.push(page);
    drawn.push(
      mount(ModelsBody, {
        target: page,
        props: {
          wire: modelsWire,
          oncheck: () => {},
          oninstall: () => {},
          onactivate: () => {},
          ondeactivate: () => {},
          onbench: () => {},
          onbenchstop: () => {},
          onarm: () => {},
        },
      }),
    );

    const sheet = readFileSync('src/assets/web.css', 'utf8');
    const dom = new JSDOM(`<style>${sheet}</style>${band.outerHTML}${page.outerHTML}`);
    const read = (selector: string): string => {
      const el = dom.window.document.querySelector(selector);
      if (el === null) throw new Error(`${selector} drew nothing`);
      return dom.window.getComputedStyle(el).clipPath;
    };

    expect(read('.band .dot.warn'), 'the band drew the verdict triangle').toBe('none');
    expect(read('.models .dot.warn'), 'the page lost its verdict triangle').toContain('polygon');
  });
});
