// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it } from 'vitest';

import Leaves from './Leaves.svelte';
import Turn from './Turn.svelte';
import { fold, type Unit, type WorkRow } from './units';

/**
 * One call, as the transcript carries it.
 *
 * The assistant frame and its result are separate messages, in arrival order,
 * which is the shape a page read gives them.
 */
const said = (uuid: string, id: string, description: string): unknown => ({
  type: 'assistant',
  uuid,
  message: {
    id: `m-${uuid}`,
    role: 'assistant',
    model: 'deepseek-v4.1-flash',
    content: [
      { type: 'tool_use', id, name: 'Bash', input: { command: 'just check', description } },
    ],
  },
});

const answered = (uuid: string, id: string): unknown => ({
  type: 'user',
  uuid,
  message: {
    role: 'user',
    content: [{ type: 'tool_result', tool_use_id: id, content: `${id} out` }],
  },
});

/** One peer message, which draws as a card named by the call's id. */
const sent = (uuid: string, id: string, words: string): unknown => ({
  type: 'assistant',
  uuid,
  message: {
    id: `m-${uuid}`,
    role: 'assistant',
    model: 'deepseek-v4.1-flash',
    content: [
      {
        type: 'tool_use',
        id,
        name: 'mcp__forge__agents__send_message',
        input: { project: 'forge', label: 'lead', message: words },
      },
    ],
  },
});

const rowsOf = (units: Unit[]): WorkRow[] =>
  units.find((unit): unit is Extract<Unit, { kind: 'leaves' }> => unit.kind === 'leaves')?.rows ??
  [];

/**
 * **The tree is reset whatever a case did**, including a case whose `mount`
 * threw `each_key_duplicate` - the failure this file exists to catch - so the
 * next case never runs against a partial DOM.
 */
let app: Record<string, unknown> | null = null;

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  document.body.innerHTML = '';
});

/**
 * **A tool_use id repeated across calls does not stop the turn drawing.**
 *
 * An engine behind the CLI can reuse a `call_...` id across separate calls:
 * measured on `Subspace/subspace/litep2p-backend` (one id carried four Read
 * calls with four different results) and on `Personal/companies/sphere-press`
 * (two ids carried five calls each). Keyed by the id alone, the second row
 * repeats the first's key, Svelte's keyed each throws `each_key_duplicate`,
 * and the whole turn is left unrendered - the page is blank. The rows BEFORE
 * the repeated call must draw too, not only the ones after it: the abort takes
 * the whole list.
 */
describe('a turn whose tool_use id repeats, mounted', () => {
  it('draws every row, before the repeat and after it', () => {
    const units = fold([
      said('a1', 'call_0_x', 'first peek'),
      answered('u1', 'call_0_x'),
      said('a2', 'call_0_y', 'the one between'),
      answered('u2', 'call_0_y'),
      // The engine reuses the first call's id for a new call with its own
      // command and its own result.
      said('a3', 'call_0_x', 'second peek'),
      answered('u3', 'call_0_x'),
      said('a4', 'call_0_z', 'after the repeat'),
      answered('u4', 'call_0_z'),
    ]);
    const rows = rowsOf(units);
    // **The mount is the repro**: on the code this fixes, the duplicate keys
    // reach Svelte's keyed each here and the mount throws `each_key_duplicate`,
    // before any assertion below runs.
    app = mount(Leaves, { target: document.body, props: { rows } });
    flushSync();
    const text = document.body.textContent ?? '';
    expect(document.querySelectorAll('details.leaf'), 'one row each').toHaveLength(4);
    expect(text, 'the row before the repeated id drew').toContain('first peek');
    expect(text, 'the row between drew').toContain('the one between');
    expect(text, 'the repeated id drew its own row').toContain('second peek');
    expect(text, 'and the row after drew').toContain('after the repeat');

    const calls = rows.filter(
      (row): row is Extract<WorkRow, { tag: 'call' }> => row.tag === 'call',
    );
    expect(
      calls.map((call) => call.key),
      'four calls, and no two rows share a key',
    ).toEqual(['c-call_0_x#a1#0', 'c-call_0_y#a2#0', 'c-call_0_x#a3#0', 'c-call_0_z#a4#0']);
  });

  /**
   * **A card is named by the call id too.** The fold records the frame and
   * block the card arrived in beside id, so the two cards' row keys and their
   * units' keys are both distinct.
   */
  it('draws both peer cards when one call id names two of them', () => {
    const units = fold([
      sent('a1', 'call_0_x', 'first note'),
      answered('u1', 'call_0_x'),
      sent('a2', 'call_0_x', 'second note'),
      answered('u2', 'call_0_x'),
    ]);
    const rows = rowsOf(units);
    expect(
      rows.filter((row) => row.tag === 'card'),
      'two cards folded',
    ).toHaveLength(2);

    app = mount(Leaves, { target: document.body, props: { rows } });
    flushSync();
    const text = document.body.textContent ?? '';
    expect(document.querySelectorAll('details.leaf'), 'both rows drew').toHaveLength(2);
    expect(text, 'the first note drew').toContain('first note');
    expect(text, 'and the second did too').toContain('second note');
  });

  /**
   * **The unit above a card is keyed by the card's own name too.** Two cards
   * under one call id, split by a prose block, land in two leaves units - and
   * a unit key built from the wire id alone would throw at the TURN's each
   * before either card drew.
   */
  it('draws two same-id cards split by prose, one container up', () => {
    const messages = [
      sent('a1', 'call_0_x', 'first note'),
      answered('u1', 'call_0_x'),
      {
        type: 'assistant',
        uuid: 'a2',
        message: {
          id: 'm-a2',
          role: 'assistant',
          model: 'deepseek-v4.1-flash',
          content: [{ type: 'text', text: 'between them' }],
        },
      },
      sent('a3', 'call_0_x', 'second note'),
      answered('u3', 'call_0_x'),
    ];

    app = mount(Turn, {
      target: document.body,
      props: { turn: { key: 'turn-1', messages, live: false } },
    });
    flushSync();
    const text = document.body.textContent ?? '';
    expect(document.querySelectorAll('details.leaf'), 'both cards drew').toHaveLength(2);
    expect(text, 'the first note drew').toContain('first note');
    expect(text, 'and the second did too').toContain('second note');
    // The prose really split the run, so the two leaves units are what the
    // TURN keys - not one merged unit whose key was never exercised.
    const units = [...document.querySelectorAll('.unit[data-k]')].map((unit) =>
      unit.getAttribute('data-k'),
    );
    expect(units, 'leaves, prose, leaves - three units, three distinct keys').toEqual([
      'turn-1:p-call_0_x#a1#0',
      'turn-1:a2#0',
      'turn-1:p-call_0_x#a3#0',
    ]);
  });
});
