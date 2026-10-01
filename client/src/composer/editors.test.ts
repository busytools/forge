import { describe, expect, it } from 'vitest';

import { focusOf, type Where } from './editors';

/**
 * The keyboard's route between the client's boxes.
 *
 * The transitions are the spec's §3 table, and each test names the property it
 * guards so a failure says which one broke rather than where it landed.
 */
describe('which editor holds the keyboard', () => {
  it('gives it to the dock while a prompt is pending', () => {
    const where: Where = { editor: 'composer', remember: 'composer', pending: true };
    expect(focusOf(where), 'a pending prompt owns the keyboard').toBe('dock');
  });

  it('gives it back to what held it when the queue drains', () => {
    const where: Where = { editor: 'dock', remember: 'composer', pending: false };
    expect(focusOf(where), 'the queue draining returns it where it was').toBe('composer');
  });

  it('lands on a surface that exists when the remembered one is gone', () => {
    const where: Where = {
      editor: 'dock',
      remember: 'connect',
      pending: false,
      connectPresent: false,
    };
    expect(focusOf(where), 'a remembered surface that is gone falls back to the composer').toBe(
      'composer',
    );
  });

  it('gives it to the connect field when only that is up', () => {
    const where: Where = {
      editor: 'connect',
      remember: 'connect',
      pending: false,
      connectPresent: true,
    };
    expect(focusOf(where), 'the connect route owns the keyboard on its own screen').toBe('connect');
  });

  it('names nowhere rather than an editor that is not mounted', () => {
    const where: Where = {
      editor: 'composer',
      remember: 'composer',
      pending: false,
      composerPresent: false,
    };
    expect(focusOf(where), 'no mounted editor is nowhere, not a stale name').toBe('nowhere');
  });
});
