// @vitest-environment jsdom
/**
 * The instrument for #1523: the composer's `ring` classes recorded across one
 * take's whole life, with the frames the live page hands the component.
 *
 * A state machine is not visible in a screenshot and not in a markup
 * assertion, because it is a sequence - so this file drives the sequence,
 * records what the class list is at each step, and asserts on the step's own
 * record rather than on where the walk happened to end. The landed beat is a
 * one-shot (the book states 450ms), so the two states the ring must never show
 * are `done` after that window and `done` over a live take.
 *
 * The two walks differ in one thing: whether a frame arrives inside the beat's
 * window. Read as a pair, they pin that the window closes on the clock rather
 * than on a frame - a beat a later hand-over happens to clear would pass the
 * walk that ends after one.
 */
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, expect, it, vi } from 'vitest';

import Harness from './Harness.svelte';
import { record, take, wire } from './testing';
import type { ComposerRecord } from './view';

let app: Record<string, unknown> | null = null;

afterEach(() => {
  if (app !== null) void unmount(app);
  app = null;
  document.body.innerHTML = '';
  vi.useRealTimers();
});

interface Page {
  record: ComposerRecord;
}

/** Mount the composer on the harness, which is what a page re-render is. */
function open(): Page {
  const shared = wire();
  app = mount(Harness, {
    target: document.body,
    props: { wire: shared, initial: { dictation: true }, dictation: true },
  });
  flushSync();
  const page = app['page'];
  if (page === null || typeof page !== 'object') throw new Error('the harness exposed no props');
  return page as Page;
}

/** The classes the ring carries right now, as one line a trace can hold. */
function ringClasses(): string {
  const box = document.querySelector('.box');
  return [...(box?.classList ?? [])].join('.');
}

/** What the dictation row inside the box says it is doing, or `-` when there is none. */
function rowState(): string {
  const row = document.querySelector('.box .dict');
  if (row === null) return '-';
  const label = row.querySelector('.lbl')?.textContent?.trim() ?? '';
  const dot = row.querySelector('.dot');
  return `${label}${dot?.classList.contains('tr') === true ? ' (tr dot)' : ' (rec dot)'}`;
}

/** A record in the given composer state, as the wire's frames leave it. */
function held(composer: Record<string, unknown>): ComposerRecord {
  return record({ composer: { compacting: false, sign_in: null, ...composer } });
}

const LANDED = { kind: 'landed', text: 'the words', truncated: false };

/** One step of a walk: what it is, and what the ring and the row were at it. */
interface Leg {
  step: string;
  ring: string;
  row: string;
}

/**
 * The walk indexed by step, or a failure naming a step recorded twice - a
 * `find` would read the first match and say nothing about the second.
 */
function steps(trace: Leg[]): Map<string, Leg> {
  const held = new Map<string, Leg>();
  for (const leg of trace) {
    if (held.has(leg.step)) throw new Error(`the walk recorded "${leg.step}" twice`);
    held.set(leg.step, leg);
  }
  return held;
}

/** The leg one step recorded, or a failure naming the step that is missing. */
function at(walk: Map<string, Leg>, step: string): Leg {
  const leg = walk.get(step);
  if (leg === undefined) throw new Error(`the walk recorded no "${step}" step`);
  return leg;
}

/** The walk as the run's own output, which is what a reader reads the sequence off. */
function printed(trace: Leg[]): string {
  return trace
    .map(
      (leg) => `${leg.step.padEnd(34)} | ring: ${(leg.ring || '-').padEnd(14)} | row: ${leg.row}`,
    )
    .join('\n');
}

/**
 * Walk a take from idle to landed and then hand over one more frame, which is
 * what the live page does: every frame re-creates the record the composer
 * reads, so the landed notice arrives as a NEW object on each hand-over.
 */
function takeLifecycle(page: Page, extraFrame: boolean): Leg[] {
  const trace: Leg[] = [];
  const note = (step: string): void => {
    trace.push({ step, ring: ringClasses(), row: rowState() });
  };

  note('idle');
  page.record = held({ take: take(), notice: null });
  flushSync();
  note('recording · take started');
  page.record = held({ take: take({ levels: [0.2, 0.5, 1, 0.9] }), notice: null });
  flushSync();
  note('recording · level frame');
  page.record = held({ take: take({ phase: 'transcribing', progress: [2, 6] }), notice: null });
  flushSync();
  note('transcribing');
  page.record = held({ take: null, notice: LANDED });
  flushSync();
  note('landed');
  if (extraFrame) {
    // Inside the beat window: the page hands the same landed notice over
    // again, as any frame arriving before the window closes does.
    page.record = held({ take: null, notice: { ...LANDED } });
    flushSync();
    note('landed + one more frame');
  }
  vi.advanceTimersByTime(2_000);
  flushSync();
  note('beat window past');
  page.record = held({ take: take(), notice: null });
  flushSync();
  note('recording · next take');

  return trace;
}

it('the control: with no frame inside the window, the landed beat expires on its own', () => {
  vi.useFakeTimers();
  const page = open();
  const trace = takeLifecycle(page, false);
  console.log(`\n#1523 control (no extra frame)\n${printed(trace)}`);

  const walk = steps(trace);
  expect(at(walk, 'landed').ring, 'the landing opens the beat').toContain('done');
  expect(at(walk, 'beat window past').ring, 'the window closes on the clock alone').not.toContain(
    'done',
  );
  expect(at(walk, 'recording · next take').ring, 'the next take is a recording').toContain('rec');
});

it('a live take owns the ring, not a landed beat still inside its window', () => {
  vi.useFakeTimers();
  const page = open();
  page.record = held({ take: take(), notice: null });
  flushSync();
  page.record = held({ take: null, notice: { ...LANDED } });
  flushSync();
  expect(ringClasses(), 'the landing opens the beat').toContain('done');

  // Back to back: the next take starts inside the beat's own window.
  page.record = held({ take: take(), notice: null });
  flushSync();

  expect(ringClasses(), 'a live take owns the ring').toContain('rec');
  expect(ringClasses(), 'and the landed beat does not paint over it').not.toContain('done');
});

it('records the ring across a take whose landed notice keeps being re-handed', () => {
  vi.useFakeTimers();
  const page = open();
  const trace = takeLifecycle(page, true);
  console.log(`\n#1523 instrument (one extra frame inside the window)\n${printed(trace)}`);

  const walk = steps(trace);
  expect(at(walk, 'landed').ring, 'the landing opens the beat').toContain('done');
  expect(
    at(walk, 'landed + one more frame').ring,
    'and a re-handed notice inside the window does not cut the beat short',
  ).toContain('done');
  expect(
    at(walk, 'beat window past').ring,
    'and the window closes even though a frame re-handed the notice inside it',
  ).not.toContain('done');
  expect(at(walk, 'recording · next take').ring, 'and the next take is a recording').toContain(
    'rec',
  );
  expect(at(walk, 'recording · next take').ring, 'a recording, not a landed one').not.toContain(
    'done',
  );
});
