/**
 * The microphone worklet's own half: it gathers render quanta into 20 ms
 * chunks, and a take can ask it for the samples it is still holding - the
 * only way the newest audio survives the graph being stopped.
 */

import { describe, expect, it } from 'vitest';

/** What the worklet instance needs of the page's own audio thread. */
interface Worklet {
  port: { postMessage: (value: Float32Array) => void; onmessage: (() => void) | null };
  process(inputs: Float32Array[][]): boolean;
}

/** Every chunk the worklet has posted, across the whole file. */
const posted: Float32Array[] = [];

let Made: (new () => Worklet) | null = null;

/**
 * The worklet as `addModule` would load it.
 *
 * The audio thread's globals have to be in place before the file runs - the
 * browser provides them there, and this is the one place the test stands
 * them in - so the module is loaded once, on first use.
 */
async function loadWorklet(): Promise<new () => Worklet> {
  if (Made !== null) return Made;
  const scope = globalThis as unknown as Record<string, unknown>;
  scope.AudioWorkletProcessor = class {
    port = { postMessage: (value: Float32Array) => posted.push(value), onmessage: null };
  };
  const box: { made: (new () => Worklet) | null } = { made: null };
  scope.registerProcessor = (name: string, processor: new () => Worklet) => {
    if (name === 'forge-mic') box.made = processor;
  };
  await import('./mic-worklet.js');
  if (box.made === null) throw new Error('the worklet did not register itself');
  Made = box.made;
  return Made;
}

describe('the microphone worklet', () => {
  it('posts a chunk as soon as it has 20 ms of samples', async () => {
    const Processor = await loadWorklet();
    posted.length = 0;
    const worklet = new Processor();

    worklet.process([[new Float32Array(320).fill(0.5)]]);
    expect(posted, 'a full chunk goes without being asked').toHaveLength(1);
    expect(posted[0]?.length).toBe(320);
  });

  it('holds a partial chunk, and posts it whole when the take asks', async () => {
    const Processor = await loadWorklet();
    posted.length = 0;
    const worklet = new Processor();

    worklet.process([[new Float32Array(128).fill(0.5)]]);
    expect(posted, 'a partial chunk is not a frame yet').toHaveLength(0);

    worklet.port.onmessage?.();
    expect(posted, 'the ask posts what is held').toHaveLength(1);
    expect(posted[0]?.length, 'the partial goes as it stands, not padded').toBe(128);

    worklet.process([[new Float32Array(128).fill(0.5)]]);
    expect(posted, 'the flushed samples are not posted a second time').toHaveLength(1);
    worklet.process([[new Float32Array(64).fill(0.5)]]);
    expect(posted, 'and the next chunk fills from empty, not from what went').toHaveLength(1);
  });
});
