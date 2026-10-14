// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it } from 'vitest';

import { extensionsWire, fakePage, type FakePage } from './testing';
import Extensions from './Extensions.svelte';

/**
 * The page's FRAME handling, which a Body mounted with fixed props cannot
 * exercise: the failure line a `plugins_cli_action_failed` draws, the press
 * feedback clearing on the frames that answer a press, and an add's own
 * outcome reopening its input.
 */
let app: Record<string, unknown> | null = null;
let fake: FakePage | null = null;

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  fake = null;
  document.body.innerHTML = '';
});

function draw(): FakePage {
  fake = fakePage(extensionsWire());
  app = mount(Extensions, { target: document.body, props: { connection: fake.connection } });
  flushSync();
  return fake;
}

function buttonByText(text: string): HTMLButtonElement {
  const found = [...document.querySelectorAll('button')].find(
    (button) => button.textContent?.trim() === text,
  );
  if (!(found instanceof HTMLButtonElement)) throw new Error(`no button reads "${text}"`);
  return found;
}

/** The Marketplaces pane, where the add control lives. */
function openMarketplaces(): void {
  const item = [...document.querySelectorAll('.ext-rail-item')].find((row) =>
    row.textContent?.includes('Marketplaces'),
  );
  if (!(item instanceof HTMLElement)) throw new Error('no Marketplaces rail item');
  item.click();
  flushSync();
}

describe('the extensions page over a live connection', () => {
  it("draws a failed action in the core's own words", () => {
    const page = draw();
    expect(document.body.textContent).not.toContain('the marketplace refused');

    page.emit({
      plugins_cli_action_failed: { cwd_raw: '/tmp/proj', message: 'the marketplace refused it' },
    });
    flushSync();

    const line = document.querySelector('.ext-said');
    expect(line?.textContent).toContain('the marketplace refused it');
  });

  it('unlatches a press when the frames that answer it land', () => {
    const page = draw();
    const update = [...document.querySelectorAll('button')].find(
      (button) => button.textContent?.trim() === 'update',
    );
    if (!(update instanceof HTMLButtonElement)) throw new Error('no update chip');
    update.click();
    flushSync();
    expect(document.body.textContent).toContain('updating...');

    // The action's own landing - any `plugins_*` frame - is what says the
    // press is over; waiting for a read would leave the chip stuck.
    page.emit({
      plugins_cli_action_succeeded: {
        cwd_raw: '/tmp/proj',
        result: { snapshot: null, message: 'updated', claude_path: '/usr/bin/claude' },
      },
    });
    flushSync();
    expect(document.body.textContent, 'the chip went back to its action').not.toContain(
      'updating...',
    );
    expect(buttonByText('update'), 'and the row offers the action again').not.toBeNull();
  });

  it("keeps a rejected add's source and its words", () => {
    const page = draw();
    openMarketplaces();
    buttonByText('add marketplace').click();
    flushSync();
    const field = document.querySelector<HTMLInputElement>('.ext-add-in');
    if (field === null) throw new Error('no add field');
    field.value = 'owner/repo';
    field.dispatchEvent(new Event('input', { bubbles: true }));
    flushSync();
    buttonByText('add').click();
    flushSync();

    page.emit({
      plugins_cli_action_failed: { cwd_raw: '/tmp/proj', message: 'no such repository' },
    });
    flushSync();

    const reopened = document.querySelector<HTMLInputElement>('.ext-add-in');
    expect(reopened, 'the rejected source reopens its input').not.toBeNull();
    expect(reopened?.value).toBe('owner/repo');
    expect(document.body.textContent).toContain('no such repository');
  });

  it('reopens the add input with its source when the dispatch is refused', () => {
    const page = draw();
    openMarketplaces();
    buttonByText('add marketplace').click();
    flushSync();
    const field = document.querySelector<HTMLInputElement>('.ext-add-in');
    if (field === null) throw new Error('no add field');
    field.value = 'owner/refused';
    field.dispatchEvent(new Event('input', { bubbles: true }));
    flushSync();
    buttonByText('add').click();
    flushSync();
    expect(document.querySelector('.ext-add-in'), 'the submit closes the input').toBeNull();

    // A refusal produces no plugin frame, so the refusal IS the add's whole
    // outcome: the source comes back under the core's own words, at once.
    page.say({ kind: 'error', what: 'dispatch', why: 'a second plugin action is running' });
    flushSync();

    const reopened = document.querySelector<HTMLInputElement>('.ext-add-in');
    expect(reopened, 'the refused source reopens its input').not.toBeNull();
    expect(reopened?.value).toBe('owner/refused');
    expect(document.body.textContent).toContain('a second plugin action is running');
  });

  it("does not drop a refused add's source on some later success", () => {
    const page = draw();
    openMarketplaces();
    buttonByText('add marketplace').click();
    flushSync();
    const field = document.querySelector<HTMLInputElement>('.ext-add-in');
    if (field === null) throw new Error('no add field');
    field.value = 'owner/refused';
    field.dispatchEvent(new Event('input', { bubbles: true }));
    flushSync();
    buttonByText('add').click();
    flushSync();
    page.say({ kind: 'error', what: 'dispatch', why: 'a second plugin action is running' });
    flushSync();

    // An unrelated action's success must not be what disposes of the source:
    // the refusal already answered for it, and the words stay in the input.
    page.emit({
      plugins_cli_action_succeeded: {
        cwd_raw: '/tmp/proj',
        result: { snapshot: null, message: 'checked', claude_path: '/usr/bin/claude' },
      },
    });
    flushSync();

    const kept = document.querySelector<HTMLInputElement>('.ext-add-in');
    expect(kept, 'the refused source survives a later success').not.toBeNull();
    expect(kept?.value).toBe('owner/refused');
  });

  it("does not let a sibling action's refusal reopen the add input", () => {
    const page = draw();
    openMarketplaces();
    buttonByText('add marketplace').click();
    flushSync();
    const field = document.querySelector<HTMLInputElement>('.ext-add-in');
    if (field === null) throw new Error('no add field');
    field.value = 'owner/in-flight';
    field.dispatchEvent(new Event('input', { bubbles: true }));
    flushSync();
    buttonByText('add').click();
    flushSync();

    // The add is in flight (the core accepted it; the CLI runs for seconds).
    // A sibling action pressed in that window is refused by the core, and
    // that refusal is the sibling's own: the add's latch must not ride it.
    buttonByText('repair').click();
    flushSync();
    page.say({ kind: 'error', what: 'dispatch', why: 'a plugin action is already running' });
    flushSync();
    expect(document.body.textContent, "the sibling's refusal names its own reason").toContain(
      'a plugin action is already running',
    );
    expect(
      document.querySelector('.ext-add-in'),
      "a sibling's refusal does not reopen the add input",
    ).toBeNull();

    // The add's own success then lands - the input stays closed, with no
    // draft left open for a duplicate re-submit.
    page.emit({
      plugins_cli_action_succeeded: {
        cwd_raw: '/tmp/proj',
        result: { snapshot: null, message: 'added', claude_path: '/usr/bin/claude' },
      },
    });
    flushSync();
    expect(
      document.querySelector('.ext-add-in'),
      "the add's own success leaves the input closed",
    ).toBeNull();
  });

  it('does not reopen a settled add on some later failure', () => {
    const page = draw();
    openMarketplaces();
    buttonByText('add marketplace').click();
    flushSync();
    const field = document.querySelector<HTMLInputElement>('.ext-add-in');
    if (field === null) throw new Error('no add field');
    field.value = 'owner/ok';
    field.dispatchEvent(new Event('input', { bubbles: true }));
    flushSync();
    buttonByText('add').click();
    flushSync();

    // The add's own landing, then an UNRELATED failure: the settled source
    // must not ride a later action's error.
    page.emit({
      plugins_cli_action_succeeded: {
        cwd_raw: '/tmp/proj',
        result: { snapshot: null, message: 'added', claude_path: '/usr/bin/claude' },
      },
    });
    flushSync();
    page.emit({
      plugins_cli_action_failed: { cwd_raw: '/tmp/proj', message: 'a later update failed' },
    });
    flushSync();

    expect(document.querySelector('.ext-add-in'), 'the add input stays closed').toBeNull();
    expect(document.body.textContent).toContain('a later update failed');
  });
});
