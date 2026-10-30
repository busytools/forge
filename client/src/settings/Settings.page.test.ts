// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it } from 'vitest';

import Settings from './Settings.svelte';
import { settingsFrame } from './frame';
import { emptyHome, settingsWire } from './testing';

/** The route component's own states: waiting for the frame, a frame this
 *  client could not read, and the document. */
let app: Record<string, unknown> | null = null;

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  settingsFrame.set({ status: 'waiting' });
  document.body.innerHTML = '';
});

function draw(): void {
  app = mount(Settings, {
    target: document.body,
    props: { home: emptyHome, mark: null },
  });
  flushSync();
}

describe('the settings route', () => {
  it('draws waiting until a frame lands', () => {
    draw();
    expect(document.body.textContent).toContain('Reading the effective config');
    expect(document.querySelector('.settings-rail'), 'no sub-nav before a document').toBeNull();
  });

  it('draws a frame this client could not read as a failed read', () => {
    settingsFrame.set({
      status: 'failed',
      detail: 'This forge sent a settings frame this client could not read.',
    });
    draw();
    expect(document.body.textContent).toContain('could not read');
  });

  it('draws the document once a frame lands', () => {
    settingsFrame.set({ status: 'ready', document: settingsWire });
    draw();
    expect(document.querySelector('.settings-rail'), 'the sub-nav drew').not.toBeNull();
    expect(document.body.textContent, 'the header names the source file').toContain(
      '~/.claude/forge/forge.toml',
    );
    expect(document.body.textContent, 'and the key total').toContain('7');
  });
});
