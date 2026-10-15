/**
 * The desktop's capture source, against a shell that is a fake: what it asks
 * the shell for, and in what order.
 */

import { beforeEach, describe, expect, it, vi } from 'vitest';

const shell = vi.hoisted(() => {
  const invoked: { command: string; args: unknown }[] = [];
  const hearing: { event: string; handler: ((event: { payload: number[] }) => void) | null }[] = [];
  /** What the shell answers the NEXT start with, when a case wants a refusal. */
  const refuses: { why: string | null; hold: string | null } = { why: null, hold: null };
  /** The input the shell reports for the NEXT start, the way `client_mic_start` does. */
  const opens: { resolved: { id: string; label: string } | null } = { resolved: null };
  /** Whether this run is the shell's own leg, which is what the hold needs. */
  const inTauri = { held: true };
  return {
    invoked,
    hearing,
    refuses,
    opens,
    inTauri,
    isTauri: (): boolean => inTauri.held,
    invoke: (command: string, args?: unknown): Promise<unknown> => {
      invoked.push({ command, args });
      if (command === 'client_mic_hold' && refuses.hold !== null) {
        return Promise.reject(new Error(refuses.hold));
      }
      if (command === 'client_mic_start') {
        if (refuses.why !== null) return Promise.reject(new Error(refuses.why));
        return Promise.resolve(opens.resolved);
      }
      return Promise.resolve(undefined);
    },
    listen: (
      event: string,
      handler: (event: { payload: number[] }) => void,
    ): Promise<() => void> => {
      const entry: { event: string; handler: ((event: { payload: number[] }) => void) | null } = {
        event,
        handler,
      };
      hearing.push(entry);
      return Promise.resolve(() => {
        entry.handler = null;
      });
    },
  };
});

vi.mock('@tauri-apps/api/core', () => ({
  invoke: shell.invoke,
  isTauri: shell.isTauri,
}));
vi.mock('@tauri-apps/api/event', () => ({ listen: shell.listen }));

import { holdShellMic, ShellMic } from './mic-shell';

/** The live `client://mic` handler, or a failure if none is registered. */
function handler(): (event: { payload: number[] }) => void {
  const live = shell.hearing.find((entry) => entry.handler !== null);
  if (live?.handler === null || live === undefined) throw new Error('no listener is up');
  return live.handler;
}

beforeEach(() => {
  shell.invoked.length = 0;
  shell.hearing.length = 0;
  shell.opens.resolved = null;
  shell.refuses.why = null;
  shell.refuses.hold = null;
  shell.inTauri.held = true;
});

describe('the shell capture', () => {
  it('listens before it asks the shell to start, so no frame is missed', async () => {
    await ShellMic.open('coreaudio:one');
    expect(shell.hearing, 'the listener is up first').toHaveLength(1);
    expect(shell.invoked.map((call) => call.command)).toEqual(['client_mic_start']);
    expect(shell.invoked[0]?.args).toMatchObject({ device: 'coreaudio:one' });
  });

  it('states the same page id on the hold and the press', async () => {
    await holdShellMic('coreaudio:one');
    await ShellMic.open('coreaudio:one');
    const pages = shell.invoked.map((call) => (call.args as { page?: unknown }).page);
    expect(pages[0], 'a hold states the page load it comes from').toEqual(expect.any(String));
    expect(
      pages[1],
      'and the press states the same one: an id minted per call would read as a new page, and a pick change mid-take would end its own recording',
    ).toEqual(pages[0]);
  });

  it('reports the input the shell opened, the one place a plain take names the default', async () => {
    shell.opens.resolved = { id: 'coreaudio:builtin', label: 'MacBook Pro Microphone' };
    const mic = await ShellMic.open(null);

    expect(
      mic.resolved,
      'the take hands this to rememberDefaultDevice, so the panel draws the resolved row',
    ).toEqual({ id: 'coreaudio:builtin', label: 'MacBook Pro Microphone' });
  });

  it('hands the frames the shell sends to the take', async () => {
    const mic = await ShellMic.open(null);
    const seen: Uint8Array[] = [];
    mic.onFrame = (bytes) => seen.push(bytes);

    handler()({ payload: [0, 1, 2] });
    handler()({ payload: [3, 4] });
    expect(seen.map((bytes) => [...bytes])).toEqual([
      [0, 1, 2],
      [3, 4],
    ]);
  });

  it('holds no partial of its own: the shell flushes on the drain', async () => {
    const mic = await ShellMic.open(null);
    expect(mic.flush()).toBeNull();
  });

  it('drains through the shell, and only then is the take closed', async () => {
    const mic = await ShellMic.open(null);
    await mic.drain();
    expect(
      shell.invoked.map((call) => call.command),
      "the drain is the shell's own: its pipeline is where the audio waits",
    ).toEqual(['client_mic_start', 'client_mic_drain']);
    const pages = shell.invoked.map((call) => (call.args as { page?: unknown }).page);
    expect(
      pages.at(-1),
      'and the drain names the page load the press did: one from a page a reload discarded ends nothing when it wakes',
    ).toEqual(pages[0]);
  });

  it('ends the take at once when the audio is being thrown away', async () => {
    const mic = await ShellMic.open(null);
    mic.stop();
    expect(shell.invoked.at(-1)?.command).toBe('client_mic_stop');
    const pages = shell.invoked.map((call) => (call.args as { page?: unknown }).page);
    expect(
      pages.at(-1),
      'and the stop names the same page load, so one in flight from a discarded page ends nothing',
    ).toEqual(pages[0]);
    expect(
      shell.hearing.every((entry) => entry.handler === null),
      'and the frames stop reaching a take that is over: the held device stays open',
    ).toBe(true);
  });

  it('takes its listener down when the shell will not open the device', async () => {
    shell.refuses.why = 'no input device named "coreaudio:gone"';
    await expect(ShellMic.open('coreaudio:gone')).rejects.toThrow('coreaudio:gone');
    expect(
      shell.hearing.every((entry) => entry.handler === null),
      'a listener left up would feed the next take frames from a device it never opened',
    ).toBe(true);
  });
});

describe('the hold', () => {
  it('keeps the picked input open, so the next press records from the press', async () => {
    await holdShellMic('coreaudio:one');
    expect(
      shell.invoked,
      'the hold is the shell opening the device and keeping it, before any take',
    ).toMatchObject([{ command: 'client_mic_hold', args: { device: 'coreaudio:one' } }]);
  });

  it('holds the system default when the pick is cleared', async () => {
    await holdShellMic(null);
    expect(
      shell.invoked[0]?.args,
      'a cleared pick is the default, which is what a press asks',
    ).toMatchObject({ device: null });
  });

  it('holds nothing in a browser, where the shell is not the one capturing', async () => {
    shell.inTauri.held = false;
    await holdShellMic('coreaudio:one');
    expect(shell.invoked, 'the web leg has no shell to hold a device in').toEqual([]);
  });

  it('is not an error when the shell will not open the device', async () => {
    shell.refuses.hold = 'no input device named "coreaudio:gone"';
    await expect(
      holdShellMic('coreaudio:gone'),
      'the press then opens the device itself, exactly as it did before there was a hold',
    ).resolves.toBeUndefined();
  });
});
