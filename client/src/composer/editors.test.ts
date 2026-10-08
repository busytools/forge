import { readFileSync, readdirSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

import { describe, expect, it } from 'vitest';

import { EDITORS, focusOf, type Editor, type Where } from './editors';

/** Whether a name a box gave itself is one of the editors, which is what makes it routable. */
function isEditor(name: string): name is Editor {
  return (EDITORS as readonly string[]).includes(name);
}

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
 *
 * It pins the attribute's mention rather than its rendering - a box that wrote
 * `data-editor={undefined}` still reads as found here - so the docking tests are
 * what prove a box actually carries it.
 *
 * Membership is pinned only where the name is written as a literal: on a raw box
 * inline, on the shared field as its prop. An expression is where the two paths
 * part - a raw box's `data-editor={...}` passes on its mention alone, which is
 * what lets the shared field's own dynamic box through, while a `Field` handed an
 * expression is an offender, its prop being the only place the name is written.
 * The docking tests are what hold each to the name it was given.
 *
 * The raw-box sweep is a text match to the tag's first `>`, so a box carrying an
 * arrow ahead of `data-editor` reads as naming none, and it reports an `input`
 * of any kind - a checkbox included. Both are false positives that fail loudly
 * and are answered by looking; neither can hide a box.
 */
describe('the census of boxes that can take text', () => {
  it('every element that can take text names an editor from the closed set', () => {
    // The dev harness drives a box rather than owning one. Read against the
    // source root and not the whole path: a checkout whose own directory is
    // called `dev` - this repo's worktrees are - matched every file.
    const files = svelteUnder(SRC).filter(
      (file) => !path.relative(SRC, file).startsWith(`dev${path.sep}`),
    );
    // A sweep that read nothing passes everything below it, which is the one
    // answer this test must never give by accident.
    expect(files.length, 'the sweep read the tree').toBeGreaterThan(40);

    const offenders: string[] = [];
    for (const file of files) {
      const where = path.relative(SRC, file);
      const text = readFileSync(file, 'utf8');

      // A raw box names its editor inline, so the name can be read here - and a
      // name outside the set is one `focusOf` can never return.
      for (const found of text.matchAll(/<(textarea|input)\b[^>]*>/g)) {
        if (!found[0].includes('data-editor')) {
          offenders.push(`${where}: ${found[0]} names no editor`);
          continue;
        }
        const named = /data-editor="([^"]*)"/.exec(found[0])?.[1];
        if (named !== undefined && !isEditor(named)) {
          offenders.push(`${where}: ${named} is not an editor`);
        }
      }

      // The shared field takes the name as a prop, which is where a surface
      // declares it - and the arrow in a `field` callback puts a `>` inside the
      // tag, so the tag is read to its own close rather than to the first one.
      for (const found of text.matchAll(/<Field\b[\s\S]*?\/>/g)) {
        const named = /editor="([^"]*)"/.exec(found[0])?.[1];
        if (named === undefined) offenders.push(`${where}: ${found[0]} names no editor`);
        else if (!isEditor(named)) offenders.push(`${where}: ${named} is not an editor`);
      }
    }

    expect(offenders, 'a box that cannot name its editor cannot be routed to').toEqual([]);
  });
});
