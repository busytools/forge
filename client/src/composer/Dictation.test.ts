// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it } from 'vitest';

import Dictation from './Dictation.svelte';
import { METER_CELLS } from '../wire/limits';
import { SLOT, take } from './testing';
import { composerFrom, type Take } from './wire';
import type { Connection } from '../socket';

const sent: Record<string, Record<string, unknown>>[] = [];

const connection = {
  dispatch(command: Record<string, Record<string, unknown>>) {
    sent.push(command);
    return null;
  },
} as unknown as Pick<Connection, 'dispatch'>;

let app: Record<string, unknown> | null = null;

afterEach(() => {
  if (app !== null) void unmount(app);
  app = null;
  sent.length = 0;
  document.body.innerHTML = '';
});

/**
 * A take as the row draws it, narrowed from the wire the way the composer
 * narrows one - so a fixture that stopped matching the wire fails here rather
 * than drawing a row of zeros.
 */
function narrowed(over: Record<string, unknown> = {}): Take {
  const held = composerFrom({ take: take(over) }).take;
  if (held === null) throw new Error('the take fixture did not narrow');
  return held;
}

function open(over: Record<string, unknown> = {}) {
  app = mount(Dictation, {
    target: document.body,
    props: { take: narrowed(over), slot: SLOT, connection },
  });
  flushSync();
}

const drawn = () => document.body.textContent ?? '';

/**
 * The row's three states, which the plan names as the part with no owner: the
 * same anatomy in both live states, and only colour and freeze change on the
 * handoff.
 */
describe('the dictation row', () => {
  it('draws the take: its dot, its clock, its level, the meter and the label', () => {
    open();

    expect(drawn(), "the clock runs off the take's own length").toContain('0:07');
    expect(drawn(), 'the live level is its own figure').toContain('-18 dB');
    expect(drawn()).toContain('listening');
    // The window is drawn full rather than as far as the take has got: the
    // bars share out the track, so a window that grew with the take would
    // resize every bar on every arriving reading.
    const cells = [...document.querySelectorAll('.wave .wtr i')];
    expect(
      cells,
      'the window is drawn at its own length, whatever the take has reported',
    ).toHaveLength(METER_CELLS);
    expect(
      cells.slice(-3).map((cell) => cell.getAttribute('style')),
      "the take's readings sit at the newest end, in order",
    ).toEqual(['height: 29%;', 'height: 54%;', 'height: 96%;']);
    expect(document.querySelector('.dict .dot'), 'recording pulses its own colour').not.toBeNull();
    expect(document.querySelector('.dict .dot')?.classList.contains('tr')).toBe(false);
  });

  it('freezes the same anatomy and dims it toward blue while transcribing', () => {
    open({ phase: 'transcribing', progress: [2, 6] });

    expect(drawn(), 'the settle tally is what a take with segments shows').toContain(
      'transcribing 2/6',
    );
    expect(document.querySelector('.wave')?.classList.contains('tr')).toBe(true);
    expect(document.querySelector('.dict .dot')?.classList.contains('tr')).toBe(true);
    expect(
      document.querySelectorAll('.wave .wtr i'),
      'the meter keeps its last readings',
    ).toHaveLength(METER_CELLS);
  });

  it('abandons the take it was drawn for', () => {
    open();

    const cancel = document.querySelector('.esc');
    if (!(cancel instanceof HTMLElement)) throw new Error('the row drew no way out');
    cancel.click();
    flushSync();

    expect(sent, 'the take is abandoned rather than submitted').toEqual([
      { dictate_stop: { key: SLOT, submit: false } },
    ]);
  });
});
