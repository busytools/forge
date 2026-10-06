import { describe, expect, it, vi } from 'vitest';

import type { BrowserAnswer, BrowserAsk, BrowserAnswerPart } from '../protocol';
import type { Connection } from '../socket';
import { bytesOf, hostTheBrowser, canHost, type HostReply } from './host';

/**
 * The mapping from what the shell's host returns to what the socket sends,
 * on its own so it can be asserted without a Tauri process: the base64 the
 * host carries is what has to become the bytes of a binary frame.
 */
describe('what the host answered', () => {
  it('turns a text part into a text part and an image into its bytes', () => {
    const reply: HostReply = {
      parts: [
        { type: 'text', text: 'navigated' },
        { type: 'image', mime_type: 'image/png', data_base64: 'AP8Q' },
      ],
    };
    expect(reply.parts.map(toAnswerPart)).toEqual([
      { type: 'text', text: 'navigated' },
      { type: 'image', mime_type: 'image/png', bytes: new Uint8Array([0x00, 0xff, 0x10]) },
    ]);
  });

  /** The bytes of a base64 string, which is what a binary frame carries. */
  it('decodes the base64 the host carries', () => {
    expect(bytesOf('')).toEqual(new Uint8Array([]));
    expect(bytesOf('AP8Q')).toEqual(new Uint8Array([0x00, 0xff, 0x10]));
  });
});

/** The mapping under test, named so the assertion above reads as the pairs. */
function toAnswerPart(part: HostReply['parts'][number]): BrowserAnswerPart {
  return part.type === 'image'
    ? { type: 'image', mime_type: part.mime_type, bytes: bytesOf(part.data_base64) }
    : part;
}

/** The seat every ask in these tests is made for. */
const SEAT = { org: 'o', project: 'p', label: 'lead' };

/** A connection that records what the host registered, and can be asked. */
function fakeConnection() {
  let handler: ((ask: BrowserAsk) => BrowserAnswer | Promise<BrowserAnswer>) | null = null;
  const connection = {
    onBrowserAsk(fn: typeof handler) {
      handler = fn;
      return () => {
        handler = null;
      };
    },
  } as unknown as Connection;
  return {
    connection,
    /**
     * One ask, through whatever the host registered. An arrow property rather
     * than a method: this is handed around on its own, and a method taken off
     * its object is the shape the lint exists to catch.
     */
    ask: async (tool: string, args: unknown = {}): Promise<BrowserAnswer> => {
      if (handler === null) throw new Error('the host registered no handler');
      return await handler({ id: 1, seat: SEAT, tool, args });
    },
  };
}

describe('the shell as the browser host', () => {
  it('answers an ask with what the shell returned, and a failure with its reason', async () => {
    const invoke = vi.fn();
    const { connection, ask } = fakeConnection();
    hostTheBrowser(connection, invoke);

    invoke.mockResolvedValueOnce({
      parts: [{ type: 'image', mime_type: 'image/png', data_base64: 'AP8Q' }],
    });
    expect(await ask('browser_take_screenshot')).toEqual({
      parts: [{ type: 'image', mime_type: 'image/png', bytes: new Uint8Array([0x00, 0xff, 0x10]) }],
    });

    // The shell answers a failed call by REJECTING with the reason, which is
    // what the tool's failure arm carries: the driver's own sentence.
    invoke.mockRejectedValueOnce('the vendored browser is not there');
    expect(await ask('browser_navigate')).toEqual({ error: 'the vendored browser is not there' });
  });

  /** The tool, its arguments and the asking seat cross to the shell verbatim:
   * the shell is what decides a named context's owner, so it has to know who
   * asked. */
  it('hands the shell the tool, the arguments and the seat the ask carried', async () => {
    const invoke = vi.fn().mockResolvedValue({ parts: [] });
    const { connection, ask } = fakeConnection();
    hostTheBrowser(connection, invoke);

    await ask('browser_navigate', { url: 'https://example.com' });
    expect(invoke).toHaveBeenCalledWith('browser_call', {
      seat: SEAT,
      tool: 'browser_navigate',
      args: { url: 'https://example.com' },
    });
  });

  /** Registration is undoable, and unregistering twice is not a mistake. */
  it('unregisters the handler it registered', async () => {
    const { connection, ask } = fakeConnection();
    const stop = hostTheBrowser(connection, vi.fn());

    stop();
    await expect(ask('browser_close')).rejects.toThrow('no handler');
  });

  /**
   * The capability is only declared where a host is really there: a page
   * opened outside the shell (the dev server in a plain browser) must not
   * claim it, because every ask it claimed would come back a failure.
   */
  it('declares the capability only inside the shell that has a host', () => {
    expect(canHost()).toBe(false);
    (globalThis as { window?: unknown }).window = { __TAURI_INTERNALS__: {} };
    expect(canHost()).toBe(true);
    (globalThis as { window?: unknown }).window = {};
    expect(canHost()).toBe(false);
  });
});
