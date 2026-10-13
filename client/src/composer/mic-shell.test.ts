/**
 * The desktop's capture source, against a shell that is a fake: what it asks
 * the shell for, and in what order.
 */

import { beforeEach, describe, expect, it, vi } from 'vitest';

const shell = vi.hoisted(() => {
  const invoked: { command: string; args: unknown }[] = [];
  const hearing: { event: string; handler: ((event: { payload: number[] }) => void) | null }[] = [];
  /** What the shell answers the NEXT start with, when a case wants a refusal. */
  const refuses: { why: string | null } = { why: null };
  /** The input the shell reports for the NEXT start, the way `client_mic_start` does. */
  const opens: { resolved: { id: string; label: string } | null } = { resolved: null };
  return {
    invoked,
    hearing,
    refuses,
    opens,
    invoke: (command: string, args?: unknown): Promise<unknown> => {
      invoked.push({ command, args });
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

vi.mock('@tauri-apps/api/core', () => ({ invoke: shell.invoke }));
vi.mock('@tauri-apps/api/event', () => ({ listen: shell.listen }));

import { ShellMic } from './mic-shell';

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
});

describe('the shell capture', () => {
  it('listens before it asks the shell to start, so no frame is missed', async () => {
    await ShellMic.open('coreaudio:one');
    expect(shell.hearing, 'the listener is up first').toHaveLength(1);
    expect(shell.invoked.map((call) => call.command)).toEqual(['client_mic_start']);
    expect(shell.invoked[0]?.args).toEqual({ device: 'coreaudio:one' });
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
  });

  it('lets the device go at once when the audio is being thrown away', async () => {
    const mic = await ShellMic.open(null);
    mic.stop();
    expect(shell.invoked.at(-1)?.command).toBe('client_mic_stop');
    expect(
      shell.hearing.every((entry) => entry.handler === null),
      'and the frames stop reaching a take that is over',
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
