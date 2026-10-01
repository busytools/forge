import { readFileSync, readdirSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

import { describe, expect, it } from 'vitest';

import { focusOf, type Where } from './editors';

/** The client's source root, so the sweep does not depend on the process cwd. */
const SRC = fileURLToPath(new URL('..', import.meta.url));

/** Every `.svelte` file under `dir`, at any depth. */
function svelteUnder(dir: string): string[] {
  const found: string[] = [];
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) found.push(...svelteUnder(full));
    else if (entry.name.endsWith('.svelte')) found.push(full);
  }
  return found;
}

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

  it('does not name the dock when the prompt has no box of its own open', () => {
    const where: Where = {
      editor: 'composer',
      remember: 'composer',
      pending: true,
      dockPresent: false,
      composerPresent: true,
    };
    expect(focusOf(where), "a dock with no box open does not take a take's words").toBe('composer');
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

/**
 * The census, by route rather than by type.
 *
 * A sweep rather than a list, because the terminal's own census is why: its
 * canonical-type grep missed a third of its surfaces, the two hand-rolled ones
 * being plain strings in another subsystem.
 */
describe('the census of boxes that can take text', () => {
  it('every element that can take text names its editor', () => {
    // The dev harness drives a box rather than owning one.
    const files = svelteUnder(SRC).filter((file) => !file.includes(`${path.sep}dev${path.sep}`));
    // A sweep that read nothing passes everything below it, which is the one
    // answer this test must never give by accident.
    expect(files.length, 'the sweep read the tree').toBeGreaterThan(40);

    const offenders: string[] = [];
    for (const file of files) {
      for (const found of readFileSync(file, 'utf8').matchAll(/<(textarea|input)\b[^>]*>/g)) {
        if (!found[0].includes('data-editor')) {
          offenders.push(`${path.relative(SRC, file)}: ${found[0]}`);
        }
      }
    }
    expect(offenders, 'a box that cannot name its editor cannot be routed to').toEqual([]);
  });
});
