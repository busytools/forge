// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it } from 'vitest';

import TakeCard from './TakeCard.svelte';
import { take } from './testing';
import { composerFrom, type Take } from './wire';

let app: Record<string, unknown> | null = null;
let cancelled = 0;

afterEach(() => {
  if (app !== null) void unmount(app);
  app = null;
  cancelled = 0;
  document.body.innerHTML = '';
});

/**
 * A take as the card draws it, narrowed from the wire the way the composer
 * narrows one - so a fixture that stopped matching the wire fails here rather
 * than drawing a card of zeros.
 */
function narrowed(over: Record<string, unknown> = {}): Take {
  const held = composerFrom({ take: take(over) }).take;
  if (held === null) throw new Error('the take fixture did not narrow');
  return held;
}

/**
 * The wire side this page produces for a take it owns.
 *
 * The levels are dBFS off this side's own frames, and the clock is its own:
 * both move here rather than riding the record, so the card draws the audio it
 * captured even while the socket is holding frames.
 */
const produced = (
  over: Partial<{
    frames: number;
    bytes: number;
    rate: number | null;
    dbfs: number[];
    elapsedMs: number;
  }> = {},
) => ({
  frames: 412,
  bytes: 257_512,
  rate: 32_768,
  dbfs: [-30, -12, 0],
  // Deliberately unlike the record's own 7 s, so a card that read the record's
  // clock instead of this side's would show the wrong figure rather than pass.
  elapsedMs: 9000,
  ...over,
});

function open(
  over: Record<string, unknown> = {},
  wire: ReturnType<typeof produced> | null = produced(),
  spread = false,
) {
  // The fixture's own phase is the row's, which is where a case states what
  // the take is doing; the record's take carries the rest.
  const phase = (over['phase'] ?? 'recording') as 'ready' | 'recording' | 'transcribing';
  app = mount(TakeCard, {
    target: document.body,
    props: {
      take: narrowed(over),
      wire,
      phase,
      oncancel: () => (cancelled += 1),
      spread,
    },
  });
  flushSync();
}

/** Swap the card drawn for another, which is one case after another in a test. */
function again(
  over: Record<string, unknown> = {},
  wire: ReturnType<typeof produced> | null = produced(),
) {
  if (app !== null) void unmount(app);
  app = null;
  document.body.innerHTML = '';
  open(over, wire);
}

const drawn = () => document.body.textContent ?? '';

/**
 * The card: one reading of the take, alive in place rather than a row that
 * grows the box - and the only place a client-captured take is drawn.
 */
describe('the take card', () => {
  it('draws the state, the clock, the graph and the wire while recording', () => {
    open();

    expect(document.querySelector('.tc .dot'), 'recording pulses its own colour').not.toBeNull();
    expect(drawn(), "the clock is this side's own, not the record's").toContain('0:09');
    expect(drawn(), 'what the capture has produced').toContain('412 fr');
    expect(drawn(), 'what the socket has taken').toContain('251.5 KB');
    expect(drawn(), 'and the pace it is leaving at').toContain('32 KB/s');

    // The graph is a fixed window of past readings, newest last, drawn at a
    // pitch the next reading cannot move - and for a take this page captured,
    // the readings are its own frames'.
    const bars = [...document.querySelectorAll('.tc .bars i')];
    expect(bars, 'the window is drawn at its own length').toHaveLength(40);
    expect(
      bars.slice(-3).map((bar) => bar.getAttribute('style')),
      "this side's own readings sit at the newest end, in order",
    ).toEqual(['height: 46%;', 'height: 76%;', 'height: 96%;']);
    expect(bars.at(-1)?.classList.contains('hot'), 'the loudest reading is the hot tone').toBe(
      true,
    );
    expect(
      document.querySelector('.tc .wave'),
      'the pill keeps its bars, not the envelope',
    ).toBeNull();
  });

  it('draws a spread card as one smooth envelope, where the pill keeps its bars', () => {
    // **The stretched graph** (Ved, 2026-10-11): at the row's width the 40
    // per-cell bars read as chunks and their 16px track flattened the
    // readings, so a card spanning a line draws the same levels as ONE path -
    // and the curve is the point: straight segments through the same readings
    // pass every other assertion here.
    open({}, produced(), true);

    expect(document.querySelector('.tc .bars'), 'no bars on a spread card').toBeNull();
    const d = document.querySelector('.tc .wave path.area')?.getAttribute('d') ?? '';
    expect(d, 'one closed envelope, curved rather than stepped').toMatch(
      /^M [\d.]+ [\d.]+ C .* L 100 100 L 0 100 Z$/,
    );
    expect(
      document.querySelector('.tc .wave path.edge')?.getAttribute('d'),
      'the edge draws the same line the area fills under',
    ).toBe(d.replace(' L 100 100 L 0 100 Z', ''));
    expect(d, 'the newest, loudest reading tops the envelope').toContain(' 100 4 L');
  });

  it("draws the record's levels and clock for a take this page did not capture", () => {
    open({}, null);

    // The one case the wire's readings are all there is: another page's take,
    // which this side has no audio for.
    const bars = [...document.querySelectorAll('.tc .bars i')];
    expect(
      bars.slice(-3).map((bar) => bar.getAttribute('style')),
      "the record's readings are the newest three here",
    ).toEqual(['height: 29%;', 'height: 54%;', 'height: 96%;']);
    expect(drawn(), "and the record's clock is what the card reads").toContain('0:07');
  });

  /**
   * A capture that is open and has produced nothing is not recording.
   *
   * The state a take passes through between its open and its first real
   * sample: a device waking, a graph starting. The row says `ready` and
   * draws no graph, so it never shows audio it does not have (#2159).
   */
  it('says ready, drawing no graph, until the capture produces', () => {
    open({ phase: 'ready' }, produced({ dbfs: [], frames: 0, bytes: 0, rate: null }));

    expect(drawn(), 'the state is named').toContain('ready');
    expect(document.querySelector('.tc .wave'), 'and nothing is drawn as audio').toBeNull();
    expect(document.querySelector('.tc .dot'), 'the take is live, so the dot stays').not.toBeNull();
    expect(document.querySelector('.tc .sr')?.textContent).toBe('the microphone is ready');
  });

  it('counts the segments ready, and draws nothing for none', () => {
    open({ progress: [2, null] });
    expect(drawn(), 'the settled count is a reading of the take').toContain('2 ready');

    void unmount(app as Record<string, unknown>);
    app = null;
    document.body.innerHTML = '';
    open({ progress: [0, null] });
    expect(document.querySelector('.tc .ready'), 'nothing ready draws no count').toBeNull();
  });

  it('counts sections in ticks while there are eight or fewer', () => {
    open({ phase: 'transcribing', progress: [2, 6] }, produced({ rate: null }));

    expect(
      document.querySelector('.tc .dot'),
      'a transcribing take is not the pulsing dot',
    ).toBeNull();
    expect(document.querySelector('.tc .spin'), 'it turns instead').not.toBeNull();
    const ticks = [...document.querySelectorAll('.tc .ticks i')];
    expect(ticks, 'one tick per section').toHaveLength(6);
    expect(
      ticks.filter((tick) => tick.classList.contains('on')),
      'the settled sections are the lit ones',
    ).toHaveLength(2);
    expect(drawn(), 'and the count is in words beside them').toContain('2 of 6');
  });

  it('fills the bar instead once the sections pass eight', () => {
    open({ phase: 'transcribing', progress: [18, 48] }, produced({ rate: null }));

    expect(
      document.querySelector('.tc .ticks'),
      'past eight a tick per section is a barcode',
    ).toBeNull();
    const fill = document.querySelector('.tc .bar > i');
    expect(fill?.getAttribute('style'), 'the fill is the share of sections settled').toBe(
      'width: 38%;',
    );
    expect(drawn()).toContain('18 of 48');
  });

  it('ticks up to exactly eight sections and fills from nine', () => {
    for (const sections of [1, 8]) {
      again({ phase: 'transcribing', progress: [1, sections] }, produced({ rate: null }));
      expect(
        document.querySelectorAll('.tc .ticks i'),
        `${String(sections)} sections are drawn as ticks`,
      ).toHaveLength(sections);
    }
    for (const sections of [9, 60]) {
      again({ phase: 'transcribing', progress: [1, sections] }, produced({ rate: null }));
      expect(
        document.querySelector('.tc .ticks'),
        `${String(sections)} sections are past the ticks`,
      ).toBeNull();
      expect(document.querySelector('.tc .bar > i'), 'and the bar carries them').not.toBeNull();
    }
  });

  it('sweeps, claiming nothing, until the section count is in', () => {
    open({ phase: 'transcribing', progress: [0, null] }, produced({ rate: null }));

    expect(document.querySelector('.tc .bar.sweep'), 'motion without a claim').not.toBeNull();
    expect(drawn()).toContain('transcribing');
  });

  it('draws no wire counts for a take this page did not capture', () => {
    open({}, null);

    expect(document.querySelector('.tc .dot'), 'the take still draws').not.toBeNull();
    expect(
      document.querySelector('.tc .fr'),
      'but its counts are the producer own fact',
    ).toBeNull();
    expect(document.querySelector('.tc .kb')).toBeNull();
    expect(document.querySelector('.tc .pace')).toBeNull();
  });

  it('abandons the take from its own way out', () => {
    open();

    const close = document.querySelector('.tc .x');
    if (!(close instanceof HTMLElement)) throw new Error('the card drew no way out');
    close.click();
    flushSync();

    expect(cancelled, 'the card hands the abandon to whoever mounted it').toBe(1);
  });
});
