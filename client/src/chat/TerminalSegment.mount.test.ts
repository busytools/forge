// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

vi.mock('../terminal/link', () => ({
  clearTerminalLink: vi.fn(),
  installTerminal: vi.fn(),
  terminalLink: () => ({
    attachable: true,
    show: vi.fn(async (_seat: unknown, id: number) => id),
    hide: vi.fn(async () => {}),
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
  if (instance !== null) unmount(instance);
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

  it('draws no row when the seat has no terminal', () => {
    const text = draw(null);
    expect(text.trim()).toBe('');
  });
});
