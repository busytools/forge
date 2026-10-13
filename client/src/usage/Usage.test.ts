// @vitest-environment jsdom
import { flushSync, mount, tick, unmount } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

import { usageFrom, type UsageReport } from '../wire/usage';
import Usage from './Usage.svelte';
import UsageBody from './UsageBody.svelte';
import { fakeConnection, USAGE, usageWire } from './testing';

const drawn: ReturnType<typeof mount>[] = [];
const hosts: HTMLElement[] = [];

afterEach(() => {
  vi.restoreAllMocks();
  for (const component of drawn.splice(0)) void unmount(component);
  for (const host of hosts.splice(0)) host.remove();
});

/** The report the fixture stands for, narrowed the way the live layer does. */
function narrowed(): UsageReport {
  return usageFrom(usageWire);
}

/** The body as it draws, mounted so a control can be reached. */
function open(
  wire: UsageReport = narrowed(),
  handlers: { onrefresh?: () => void } = {},
): HTMLElement {
  const host = document.createElement('div');
  document.body.append(host);
  hosts.push(host);
  const component = mount(UsageBody, {
    target: host,
    props: { wire, onrefresh: handlers.onrefresh ?? (() => {}) },
  });
  drawn.push(component);
  return host;
}

describe('the usage page as it draws', () => {
  /**
   * The headline is the lifetime window, and both halves of the notional
   * claim are drawn: the ≈ on the figure and the caption beside it.
   */
  it('summarises the lifetime with its caption', () => {
    const html = open().textContent ?? '';

    expect(html).toContain('lifetime');
    expect(html).toContain('451.8M');
    expect(html).toContain('$243.60');
    expect(html).toContain('1 projects \u{b7} 2 models');
    expect(html).toContain('notional \u{b7} at API pricing \u{b7} not a bill');
  });

  /**
   * **An unpriced report never draws a misleading cost.** The headline cost
   * blanks to a dash and the caption says why, in the warning tone.
   */
  it('draws an unpriced report as a dash with the pending caption', () => {
    const host = open(usageFrom({ ...usageWire, pricing_available: false }));
    const text = host.textContent ?? '';

    expect(text).toContain('451.8M');
    expect(text).toContain('pricing pending or failed');
    expect(text).not.toContain('$243.60');
    expect(host.querySelector('.head .cost.off')?.textContent, 'the cost blanks to a dash').toBe(
      '-',
    );
    expect(
      host.querySelector('.head .cap.warn'),
      'the caption takes the warning tone',
    ).not.toBeNull();
    // The rolling totals blank the same way.
    expect(host.querySelector('.roll .c')?.textContent).toBe('-');
  });

  it('draws the three rolling totals in the order the body reads them', () => {
    const text = open().textContent ?? '';

    const month = text.indexOf('this month');
    const week = text.indexOf('this week');
    const today = text.indexOf('today');
    expect(month).toBeGreaterThan(-1);
    expect(week).toBeGreaterThan(month);
    expect(today).toBeGreaterThan(week);
    expect(text).toContain('181.7M');
    expect(text).toContain('$89.90');
    expect(text).toContain('23.4M');
    expect(text).toContain('$11.40');
  });

  /** The split, with the cache share the terminal computes. */
  it('draws the lifetime split with the cache share', () => {
    const split = open().querySelector('.split')?.textContent ?? '';

    expect(split).toContain('input');
    expect(split).toContain('2.5M');
    expect(split).toContain('cache-write');
    expect(split).toContain('18.0M');
    expect(split).toContain('cache-read');
    expect(split).toContain('430.0M');
    expect(split).toContain('(95%)');
    expect(split).toContain('output');
    expect(split).toContain('1.3M');
  });

  /**
   * The table opens on the terminal's own defaults - by project, over the
   * lifetime - with the total pinned below the rows as a footer row.
   */
  it('draws the project rows by default, with the total pinned', () => {
    const host = open();
    const body = [...host.querySelectorAll('tbody th[scope="row"]')].map((th) => th.textContent);
    const total = host.querySelector('tfoot th[scope="row"]');

    expect(body).toEqual(['forge']);
    expect(total?.textContent).toBe('TOTAL');
    const first = host.querySelector('tbody tr');
    expect(first?.textContent).toContain('451.8M');
    expect(first?.textContent).toContain('$243.60');
  });

  /**
   * **The grouping pick swaps the rows in place**, and the GPT flag is the
   * model view's: a model whose price is an approximation says so, and a
   * project that merely reads GPT-shaped does not.
   */
  it('swaps to the model rows on the pick, flagging the GPT approximation', () => {
    const host = open();
    const model = [...host.querySelectorAll<HTMLButtonElement>('button.pick')].find(
      (c) => c.textContent === 'by model',
    );
    expect(model, 'the grouping pick did not draw').not.toBeUndefined();
    model?.click();
    flushSync();

    expect(model?.getAttribute('aria-pressed')).toBe('true');
    const rows = [...host.querySelectorAll('tbody tr')];
    expect(rows.map((tr) => tr.querySelector('th')?.textContent)).toEqual([
      'claude-opus-5',
      'gpt-5-codex',
    ]);
    expect(rows[0]?.textContent, 'a priced model carries its cost').toContain('$240.00');
    expect(rows[1]?.textContent, 'the approximation is named').toContain('(GPT approx)');
    // And the project view, back, carries no flag.
    const project = [...host.querySelectorAll<HTMLButtonElement>('button.pick')].find(
      (c) => c.textContent === 'by project',
    );
    project?.click();
    flushSync();
    expect(host.textContent).not.toContain('(GPT approx)');
  });

  /** The window pick redraws the table over that window's figures. */
  it('redraws the table over the picked window', () => {
    const host = open();
    const week = [...host.querySelectorAll<HTMLButtonElement>('button.pick')].find(
      (c) => c.textContent === 'week',
    );
    expect(week, 'the window pick did not draw').not.toBeUndefined();
    week?.click();
    flushSync();

    expect(week?.getAttribute('aria-pressed')).toBe('true');
    expect(host.querySelector('tbody tr')?.textContent).toContain('121.3M');
    expect(host.querySelector('tbody tr')?.textContent).toContain('$57.50');
    // The headline is the lifetime's, whatever window the table reads.
    expect(host.querySelector('.head')?.textContent).toContain('451.8M');
  });

  /** `scratch` is where tokens pooled, not a project, and it draws quiet
   * rather than like a real row. */
  it('draws the pooled labels quiet', () => {
    const host = open(
      usageFrom({
        ...usageWire,
        lifetime: {
          ...usageWire.lifetime,
          by_project: [
            {
              label: 'scratch',
              input: 900_000,
              cache_write_1h: 0,
              cache_write_5m: 0,
              cache_read: 0,
              output: 100_000,
              cost_usd: 1.0,
            },
          ],
        },
      }),
    );

    const row = host.querySelector('tbody tr');
    expect(row?.classList.contains('quiet')).toBe(true);
    expect(row?.textContent).toContain('scratch');
  });

  /**
   * **The subject hears nothing after its first scan**, so the refresh
   * press is the only way fresh figures arrive - it must reach the caller.
   */
  it('asks for a re-read when refresh is pressed', () => {
    let refreshes = 0;
    const host = open(narrowed(), { onrefresh: () => (refreshes += 1) });

    const button = host.querySelector<HTMLButtonElement>('.controls .re');
    expect(button, 'the refresh control did not draw').not.toBeNull();
    button?.click();
    flushSync();

    expect(refreshes).toBe(1);
  });
});

describe('the usage route as it draws', () => {
  /** Mount the route on a connection a test drives. */
  function route(forge: ReturnType<typeof fakeConnection>): HTMLElement {
    const host = document.createElement('div');
    document.body.append(host);
    hosts.push(host);
    const component = mount(Usage, { target: host, props: { connection: forge.connection } });
    drawn.push(component);
    return host;
  }

  /** No connection, no read: the page says it is reading rather than drawing
   * an empty report. */
  it('draws the loading line before the first read lands', () => {
    const host = document.createElement('div');
    document.body.append(host);
    hosts.push(host);
    const component = mount(Usage, { target: host, props: { connection: null } });
    drawn.push(component);

    expect(host.textContent).toContain('Reading the usage...');
    expect(host.querySelector('.sum'), 'an empty report drew in the loading state').toBeNull();
  });

  /**
   * **The page's live wiring, which no string-level test can see.** The
   * route subscribes the subject itself and hands the snapshot down - a
   * rename or a deleted effect breaks it silently, with every drawing test
   * green.
   */
  it('subscribes the subject, draws what it answers, and re-asks on refresh', async () => {
    const forge = fakeConnection();
    const host = route(forge);
    await tick();

    expect(forge.subscribed, 'the route did not subscribe the subject').toEqual([USAGE]);

    forge.arrive({ kind: 'snapshot', subject: USAGE, data: usageWire });
    await tick();
    expect(host.textContent, 'the snapshot never reached the page').toContain('$243.60');

    const button = host.querySelector<HTMLButtonElement>('.controls .re');
    button?.click();
    flushSync();
    expect(forge.refreshed, 'the press did not ask for the re-read').toEqual([USAGE]);
  });

  /** A refused subscription is the server's own words on the page. */
  it('draws the refusal in the server words', async () => {
    const forge = fakeConnection();
    const host = route(forge);
    await tick();

    forge.arrive({ kind: 'error', what: 'subscribe', why: 'no usage on this forge' });
    await tick();

    expect(host.textContent).toContain('no usage on this forge');
    expect(host.textContent, 'a refusal drew the loading line').not.toContain('Reading the usage');
  });

  /**
   * **An empty pool is its own page.** A headline of zeros over an empty
   * table reads as a report that failed, not one with nothing yet - and the
   * terminal's own overlay has the same problem in the other direction.
   */
  it('names an empty pool rather than drawing zeros', async () => {
    const forge = fakeConnection();
    const host = route(forge);
    await tick();

    const zero = {
      label: 'TOTAL',
      input: 0,
      cache_write_1h: 0,
      cache_write_5m: 0,
      cache_read: 0,
      output: 0,
      cost_usd: 0,
    };
    const empty = { by_model: [], by_project: [], total: zero };
    forge.arrive({
      kind: 'snapshot',
      subject: USAGE,
      data: {
        today: empty,
        week: empty,
        month: empty,
        lifetime: empty,
        pricing_available: false,
      },
    });
    await tick();

    expect(host.textContent).toContain('No usage yet');
    expect(host.querySelector('.sum'), 'zeros drew as a report').toBeNull();
  });

  /** **Leaving the page releases the subject.** The effect's teardown is the
   * only thing that does it, and without it a forge keeps a subscription
   * that would scan its pool on the next ask. */
  it('releases the subject when the route is left', async () => {
    const forge = fakeConnection();
    route(forge);
    await tick();
    expect(forge.subscribed).toEqual([USAGE]);

    const component = drawn.pop();
    if (component === undefined) throw new Error('the route was not mounted');
    void unmount(component);

    expect(forge.unsubscribed, 'leaving the page left the subject subscribed').toEqual([USAGE]);
    expect(forge.listening(), 'the release left a listener on the connection').toBe(0);
  });
});
