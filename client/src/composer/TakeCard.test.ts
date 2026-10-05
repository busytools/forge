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

/** The wire side this page produces for a take it owns. */
const produced = (
  over: Partial<{ frames: number; bytes: number; rate: number | null; held: number }> = {},
) => ({
  frames: 412,
  bytes: 257_512,
  rate: 32_768,
  held: 0,
  ...over,
});

function open(
  over: Record<string, unknown> = {},
  wire: ReturnType<typeof produced> | null = produced(),
) {
  app = mount(TakeCard, {
    target: document.body,
    props: { take: narrowed(over), wire, oncancel: () => (cancelled += 1) },
  });
  flushSync();
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
    expect(drawn(), "the clock runs off the take's own length").toContain('0:07');
    expect(drawn(), 'what the capture has produced').toContain('412 fr');
    expect(drawn(), 'what the socket has taken').toContain('251.5 KB');
    expect(drawn(), 'and the pace it is leaving at').toContain('32 KB/s');

    // The graph is a fixed window of past readings, newest last, drawn at a
    // pitch the next reading cannot move.
    const bars = [...document.querySelectorAll('.tc .bars i')];
    expect(bars, 'the window is drawn at its own length').toHaveLength(40);
    expect(
      bars.slice(-3).map((bar) => bar.getAttribute('style')),
      "the take's readings sit at the newest end, in order",
    ).toEqual(['height: 29%;', 'height: 54%;', 'height: 96%;']);
    expect(bars.at(-1)?.classList.contains('hot'), 'the loudest reading is the hot tone').toBe(
      true,
    );
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

  it('says a held take in words, in place of the readings that stopped', () => {
    open({}, produced({ frames: 1712, bytes: 70_656, rate: 0, held: 1212 }));

    expect(drawn(), 'the frames keep counting while the socket is down').toContain('1712 fr');
    expect(drawn(), 'and the hold is named rather than inferred').toContain('holding 1212 fr');
    expect(drawn(), 'nothing claims bytes while they are not leaving').not.toContain('KB');
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
