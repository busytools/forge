import { describe, expect, it } from 'vitest';

import { offer, type Sources } from './autocomplete';
import type { FileEntry } from './wire';

/** A file index with the shapes the ranking turns on: a basename, a path, a depth. */
function file(relPath: string): FileEntry {
  const basename = relPath.split('/').pop() ?? relPath;
  return {
    relPath,
    relPathLower: relPath.toLowerCase(),
    basenameLower: basename.toLowerCase(),
    depth: relPath.split('/').length - 1,
  };
}

/**
 * The lists, as a pull rather than a value: the caller reads them only for the
 * list it is opening, so a draft that opens nothing never builds the index.
 */
const sources = (): Sources => ({
  forgeCommands: [
    { name: '/compact', description: 'Compact session context' },
    { name: '/model', description: 'Show / set session model' },
    { name: '/memory', description: 'Edit project memory' },
  ],
  advertised: [
    { name: '/clear', description: 'Clear chat history' },
    // A name in both lists is forge's: drawing the CLI's copy beside it would
    // offer one command twice.
    { name: '/compact', description: "the CLI's own words for it" },
  ],
  files: [
    file('src/home.rs'),
    file('src/forge.rs'),
    file('crates/forge-web/src/home.rs'),
    file('docs/home-notes.md'),
    file('src/tap.rs'),
  ],
  agents: [
    { name: 'cli-version', description: 'settled 3m' },
    { name: 'cli-audit', description: 'never run' },
  ],
});

/**
 * Four triggers, one popover shape.
 *
 * The trigger rules are the terminal's, ported: a slash command is the WHOLE
 * draft while it is being typed, which is what keeps a slash inside a sentence
 * a path rather than a command, and the `:` counts only at the start of the
 * text or after whitespace, so a URL never opens a picker.
 */
describe('which list a draft opens', () => {
  it('opens on the trigger it ends in, and on nothing that only looks like one', () => {
    expect(offer('/m', sources)?.kind).toBe('command');
    expect(
      offer('ship it /m', sources),
      'a command is the whole draft while it is typed',
    ).toBeNull();
    expect(offer('look at @home', sources)?.kind).toBe('file');
    expect(offer('ask &cli', sources)?.kind).toBe('agent');
    expect(offer('nice :sm', sources)?.kind).toBe('emoji');

    expect(offer('see http://example.com', sources), 'a URL opens nothing').toBeNull();
    expect(offer('note:todo', sources), 'a colon mid-word opens nothing').toBeNull();
    expect(offer('Foo::bar', sources), 'and neither does a second colon').toBeNull();
    expect(offer('note :t', sources), 'one character selects nothing').toBeNull();
    expect(offer('look at @', sources), 'a bare trigger has nothing to match on').toBeNull();
  });

  it('names what the row writes, which is the value the draft takes', () => {
    expect(
      offer('/m', sources)?.rows.map((row) => row.insert),
      'the rows carry the names as they are typed, which is what a pick writes',
    ).toEqual(['/memory', '/model', '/compact']);
    expect(offer('@tap', sources)?.rows[0]?.insert).toBe('@src/tap.rs');
    expect(offer('&cli', sources)?.rows[0]?.insert).toBe('&cli-version');
    expect(offer(':sm', sources)?.rows[0]?.insert).toBe('\u{1F604}');
  });
});

/**
 * The two sources the `/` list draws from, which is the one list with more
 * than one. The terminal separates them with a group heading and orders
 * forge's first; a query that matches both keeps forge's in front wherever the
 * two matched equally well, and the heading goes with the filter - a header
 * over a list that has collapsed to one source reads as a list that lost the
 * other.
 */
describe("forge's commands and the CLI's", () => {
  it("leads with forge's own wherever two rows matched equally well", () => {
    expect(
      offer('/c', sources)?.rows.map((row) => row.insert),
      "the CLI's own name match leads a forge row that only matched its description",
    ).toEqual(['/compact', '/clear', '/memory']);
    expect(
      offer('/', sources)?.rows.map((row) => row.insert),
      "and a bare trigger is forge's whole group before the CLI's, which is the list Ved presses / on",
    ).toEqual(['/compact', '/memory', '/model', '/clear']);
  });

  it('heads each source while the list is whole, and neither once a query narrows it', () => {
    expect(
      offer('/', sources)?.rows.map((row) => row.group),
      'the heading sits on the first row of its group, and each group carries one',
    ).toEqual(['forge', null, null, 'cli']);
    expect(
      offer('/m', sources)?.rows.map((row) => row.group),
      'the terminal drops the dividers with the filter, and so does this',
    ).toEqual([null, null, null]);
  });
});

describe('what a list is ranked and cut by', () => {
  it('ranks a file by where the match is, then by how deep it sits, then by path', () => {
    expect(
      offer('@for', sources)?.rows.map((row) => row.text),
      'a basename match leads a path match',
    ).toEqual(['src/forge.rs', 'crates/forge-web/src/home.rs']);

    expect(
      offer('@home', sources)?.rows.map((row) => row.text),
      'and the basename matches break by depth, then alphabetically',
    ).toEqual(['docs/home-notes.md', 'src/home.rs', 'crates/forge-web/src/home.rs']);
  });

  it('ranks the emoji exact, then prefix, then substring, ties alphabetical', () => {
    expect(offer(':check', sources)?.rows[0]?.insert).toBe('\u{2714}');
    // `alarm_clock` contains `cl` and sorts first in the table, so a version
    // that handed back the table's own order would lead with it.
    expect(offer(':cl', sources)?.rows[0]?.insert, 'the prefix match leads').toBe('\u{1F44F}');
  });

  it('counts every match in the header, not the window it draws', () => {
    const many = (): Sources => ({
      ...sources(),
      files: [...Array(300).keys()].map((n) => file(`src/home${n}.rs`)),
    });
    const held = offer('@home', many);
    expect(held?.total, 'the header states how many matched').toBe(300);
    expect(held?.rows.length, 'the window scrolls over a bounded set').toBe(200);
  });
});
