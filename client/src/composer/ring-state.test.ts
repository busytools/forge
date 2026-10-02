// @vitest-environment jsdom
/**
 * The instrument for #1523: the composer's `ring` classes recorded across one
 * take's whole life, with the frames the live page hands the component.
 *
 * A state machine is not visible in a screenshot and not in a markup
 * assertion, because it is a sequence - so this file drives the sequence and
 * records what the class list is at each step. The landed beat is a one-shot
 * (the book states 450ms), so the two states the ring must never show are
 * `done` after that window and `done` over a live take.
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

/**
 * Walk a take from idle to landed and then hand over one more frame, which is
 * what the live page does: every frame re-creates the record the composer
 * reads, so the landed notice arrives as a NEW object on each hand-over.
 */
function takeLifecycle(page: Page, extraFrame: boolean): string[] {
  const trace: string[] = [];
  const note = (step: string): void => {
    trace.push(
      `${step.padEnd(34)} | ring: ${(ringClasses() || '-').padEnd(14)} | row: ${rowState()}`,
    );
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

it('the control: without the extra frame the landed beat expires on its own', () => {
  vi.useFakeTimers();
  const page = open();
  const trace = takeLifecycle(page, false);
  console.log(`\n#1523 control (no extra frame)\n${trace.join('\n')}`);

  expect(ringClasses(), 'after the window the landed beat is gone').not.toContain('done');
  expect(ringClasses(), 'and the next take is recording').toContain('rec');
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
  console.log(`\n#1523 instrument (one extra frame inside the window)\n${trace.join('\n')}`);

  const atLanding = trace.find((line) => line.startsWith('landed '));
  expect(atLanding, 'the landed beat is drawn when the take lands').toContain('done');

  expect(ringClasses(), 'after the window the landed beat is gone').not.toContain('done');
  expect(ringClasses(), 'and the next take is a recording, not a landed one').not.toContain('done');
  expect(ringClasses(), 'and the next take is a recording').toContain('rec');
});
