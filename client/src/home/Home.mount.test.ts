// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

import { homeWire } from '../dev/fixture.data';
import type { HomeWire } from '../wire/home';
import Frames from './testing/Frames.svelte';

/**
 * The home's clock, which no server-rendered test can hold: `Home.test.ts`
 * draws through `svelte/server`, where effects do not run, so the ticking
 * half of the fleet row's age cell is invisible to every assertion there.
 */
let app: Record<string, unknown> | null = null;

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  document.body.innerHTML = '';
  vi.useRealTimers();
});

/** The fleet row's age cell, and the harness's count of frames that landed. */
const age = (): string => document.querySelector('.fleet-when')?.textContent ?? '';
const frames = (): number => Number(document.querySelector('.frames-count')?.textContent ?? '0');

describe("the home's clock", () => {
  it('keeps the ages moving while frames land faster than its half-minute tick', () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date('2026-10-10T00:00:00Z'));
    // Forty-five seconds back: inside `now` at mount, and one tick short of
    // a minute - so a clock the frames froze reads `now` here where one that
    // ticks reads `1m`.
    const wrote = Math.floor(Date.now() / 1_000) - 45;
    const wire: HomeWire = {
      ...homeWire,
      agents: homeWire.agents.map((row) => ({
        ...row,
        last_activity: { secs_since_epoch: wrote, nanos_since_epoch: 0 },
      })),
    };
    app = mount(Frames, { target: document.body, props: { initial: wire, everyMs: 10_000 } });
    flushSync();
    expect(age(), 'a write forty-five seconds back reads as now').toBe('now');

    // Four frames, one every ten seconds, each flushed as the stream flushes
    // it - which is what re-runs an effect that reads the snapshot, and so
    // what kills a timer that re-arms per frame. The tick is due at thirty
    // seconds and fires there.
    for (let at = 0; at < 4; at += 1) {
      vi.advanceTimersByTime(10_000);
      flushSync();
    }

    expect(frames(), 'no frame landed, so this test proves nothing').toBeGreaterThanOrEqual(3);
    expect(age(), 'the ages froze while the frames landed').toBe('1m');
  });
});
