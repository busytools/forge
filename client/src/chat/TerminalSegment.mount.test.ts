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

function draw(terminal: TerminalSummary | null): string {
  host = document.createElement('div');
  document.body.appendChild(host);
  instance = mount(TerminalSegment, { target: host, props: { slot: seat, terminal } });
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
});
