// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

vi.mock('../terminal/link', () => ({
  clearTerminalLink: vi.fn(),
  installTerminal: vi.fn(),
  terminalLink: () => ({
    attachable: true,
    show: vi.fn((_seat: unknown, id: number) => Promise.resolve(id)),
    hide: vi.fn(() => Promise.resolve()),
    input: vi.fn(),
    resize: vi.fn(),
    pause: vi.fn(),
    close: vi.fn(),
    onFrame: () => () => {},
    onView: () => () => {},
    onState: () => () => {},
  }),
}));

import TerminalSegment from './TerminalSegment.svelte';
import type { TerminalSummary } from '../wire/terminal';

const seat = { org: 'TestOrg', project: 'proj', label: 'lead' };

function summary(over: Partial<TerminalSummary> = {}): TerminalSummary {
  return {
    id: 1,
    shell: '/bin/zsh',
    cwd: '/tmp',
    cols: 120,
    rows: 30,
    alive: true,
    state: { running: false, input_expected: null, exit: null, source: 'exact' },
    title: '',
    held: false,
    ...over,
  };
}

let instance: ReturnType<typeof mount> | null = null;
let host: HTMLElement;

afterEach(() => {
  if (instance !== null) void unmount(instance);
  instance = null;
  host.remove();
});

function draw(terminal: TerminalSummary | null, sessionless = false): string {
  host = document.createElement('div');
  document.body.appendChild(host);
  instance = mount(TerminalSegment, {
    target: host,
    props: { slot: seat, terminal, sessionless },
  });
  flushSync();
  return host.textContent ?? '';
}

describe('the terminal strip row', () => {
  it('draws the state in words and offers show', () => {
    const text = draw(summary({ state: { ...summary().state, running: true } }));
    expect(text).toContain('terminal');
    expect(text).toContain('a command is running');
    expect(text).toContain('show');
  });

  it('says a parked hand-off is waiting for the person', () => {
    const text = draw(summary({ held: true }));
    expect(text).toContain('waiting for you');
  });

  it('is the person door on a seat with no terminal', () => {
    const text = draw(null);
    expect(text).toContain('terminal');
    expect(text).toContain('off');
    expect(text).toContain('start');
  });

  it('draws no door and says why when the seat holds no session', () => {
    const text = draw(null, true);
    expect(text).toContain('terminal');
    expect(text).toContain('the seat has no session yet - start it first');
    expect(host.querySelector('button'), 'a seat with no session gets no door').toBeNull();
  });

  it('is one pill with the terminal glyph, the state dot and its door', () => {
    host = document.createElement('div');
    document.body.appendChild(host);
    instance = mount(TerminalSegment, { target: host, props: { slot: seat, terminal: summary() } });
    flushSync();
    const pill = host.querySelector('button.tm-tog');
    expect(pill, 'the row is one button').not.toBeNull();
    expect(host.querySelectorAll('button').length, 'and not a second button inside it').toBe(1);
    expect(pill?.querySelector('use')?.getAttribute('href'), 'the terminal glyph').toBe(
      '#i-square-terminal',
    );
    expect(pill?.querySelector('.dot.idle'), 'the state mark').not.toBeNull();
    expect(pill?.textContent, 'the word, the state and the door').toContain('terminal');
    expect(pill?.textContent).toContain('idle');
    expect(pill?.textContent).toContain('show');
  });
});
