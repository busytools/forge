// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

import { violationsOf } from '../testing/axe';
import type { Connection } from '../socket';
import { Stores } from '../stores';
import type { Subject } from '../protocol';
import type { SessionSlot } from '../wire/types';
import Share from './Share.svelte';
import { shareOf, worthOpening } from './pending';
import {
  MAX_SHARE_BYTES,
  composePrompt,
  sizeLabel,
  type ShareProgress,
  type SharedFile,
  type UploadFn,
} from './upload';

const LEAD: SessionSlot = { org: 'TestOrg', project: 'proj', label: 'lead' };
const WORKER: SessionSlot = { org: 'TestOrg', project: 'proj', label: 'w1' };

let app: Record<string, unknown> | null = null;

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  document.body.innerHTML = '';
});

/** A connection that records what it was told and holds no record. */
function stub() {
  const dispatched: { command: unknown; at?: SessionSlot }[] = [];
  const stores = new Stores();
  const connection = {
    dispatch: (command: unknown, at?: SessionSlot) => {
      dispatched.push(at === undefined ? { command } : { command, at });
      return Promise.resolve(null);
    },
    subscribe: (what: Subject) => stores.open(what),
    release: () => {},
    refresh: () => true,
    relist: () => {},
    declare: () => {},
    onMessage: () => () => {},
    settings: () => null,
    status: () => 'open',
    onStatus: () => () => {},
  } as unknown as Connection;
  return { connection, dispatched };
}

const FILES: SharedFile[] = [
  { path: '/cache/share/1/photo.jpg', name: 'photo.jpg', size: 10, mime: 'image/jpeg' },
];

function draw(
  over: {
    connection?: Connection;
    files?: SharedFile[];
    error?: string | null;
    empty?: boolean;
    preselect?: SessionSlot | null;
    upload?: UploadFn;
  } = {},
) {
  const { connection, dispatched } = stub();
  app = mount(Share, {
    target: document.body,
    props: {
      connection: over.connection ?? connection,
      seats: [LEAD, WORKER],
      shared: {
        files: over.files ?? FILES,
        error: over.error ?? null,
        empty: over.empty ?? false,
      },
      preselect: over.preselect ?? null,
      onclose: () => {},
      upload: over.upload ?? (() => Promise.resolve({ paths: ['/inbox/photo.jpg'] })),
    },
  });
  flushSync();
  return { dispatched };
}

const picker = () => document.querySelector<HTMLSelectElement>('.seat select');
const sendButton = () =>
  [...document.querySelectorAll<HTMLButtonElement>('button')].find(
    (button) => button.textContent?.trim() === 'send',
  ) ?? null;
const retryButton = () =>
  [...document.querySelectorAll<HTMLButtonElement>('button')].find(
    (button) => button.textContent?.trim() === 'retry',
  ) ?? null;
const lines = () =>
  [...document.querySelectorAll<HTMLElement>('.line')].map((line) => line.textContent ?? '');

describe('the share sheet', () => {
  it('preselects the seat the reader was last on', () => {
    draw({ preselect: WORKER });
    expect(picker()?.value).toBe([WORKER.org, WORKER.project, WORKER.label].join('\u0000'));
    expect(picker()?.selectedOptions[0]?.textContent).toBe('proj \u{b7} w1');
  });

  it('opens on the first seat when there is no last one', () => {
    draw();
    expect(picker()?.value).toBe([LEAD.org, LEAD.project, LEAD.label].join('\u0000'));
  });

  it('sends to the seat the picker was moved to', async () => {
    const upload = vi.fn((_seat: SessionSlot, _files: SharedFile[], _id: number | null) =>
      Promise.resolve({ paths: ['/inbox/photo.jpg'] }),
    );
    const { dispatched } = draw({ upload });
    const select = picker();
    if (select === null) throw new Error('the seat picker is not on the page');
    select.value = [WORKER.org, WORKER.project, WORKER.label].join('\u0000');
    select.dispatchEvent(new Event('change', { bubbles: true }));
    flushSync();

    sendButton()?.click();
    await vi.waitFor(() => {
      flushSync();
      expect(dispatched).toHaveLength(1);
    });
    expect(upload.mock.calls[0]?.[0]).toEqual(WORKER);
    expect(dispatched[0]?.at).toEqual(WORKER);
  });

  it('draws the resume line and the progress the upload reports', async () => {
    let report: (progress: ShareProgress) => void = () => {};
    const upload = vi.fn(
      (
        _seat: SessionSlot,
        _files: unknown,
        _id: number | null,
        onProgress: (p: ShareProgress) => void,
      ) => {
        report = onProgress;
        return new Promise<{ paths: string[] }>(() => {});
      },
    );
    draw({ upload });
    sendButton()?.click();
    flushSync();
    report({ kind: 'begin', upload_id: 7, attempted: true, resumed: true, received: [0] });
    report({ kind: 'progress', file: 0, sent: 5, total: 10 });
    flushSync();
    expect(lines().some((line) => line.includes("resuming from the server's offsets"))).toBe(true);
    expect(lines().some((line) => line.includes('50%'))).toBe(true);
    expect(await violationsOf(document.body.innerHTML)).toEqual([]);
  });

  it('says when it is starting over', () => {
    let report: (progress: ShareProgress) => void = () => {};
    const upload = vi.fn(
      (
        _seat: SessionSlot,
        _files: unknown,
        _id: number | null,
        onProgress: (p: ShareProgress) => void,
      ) => {
        report = onProgress;
        return new Promise<{ paths: string[] }>(() => {});
      },
    );
    draw({ upload });
    sendButton()?.click();
    flushSync();
    report({ kind: 'begin', upload_id: 7, attempted: true, resumed: false, received: [0] });
    flushSync();
    expect(lines().some((line) => line.includes('starting over from the first byte'))).toBe(true);
  });

  /// **A first send draws no resume line.** `attempted: false` is the fresh
  /// begin, and a "starting over" line over it would describe an attempt
  /// that never happened.
  it('draws no resume line on a first send', () => {
    let report: (progress: ShareProgress) => void = () => {};
    const upload = vi.fn(
      (
        _seat: SessionSlot,
        _files: unknown,
        _id: number | null,
        onProgress: (p: ShareProgress) => void,
      ) => {
        report = onProgress;
        return new Promise<{ paths: string[] }>(() => {});
      },
    );
    draw({ upload });
    sendButton()?.click();
    flushSync();
    report({ kind: 'begin', upload_id: 7, attempted: false, resumed: false, received: [0] });
    flushSync();
    expect(lines().some((line) => line.includes('starting over'))).toBe(false);
    expect(lines().some((line) => line.includes('resuming'))).toBe(false);
  });

  it('draws the reason and a retry when the upload fails', async () => {
    const upload = vi.fn(() => Promise.reject(new Error('the socket is not open')));
    draw({ upload });
    sendButton()?.click();
    await Promise.resolve();
    await Promise.resolve();
    flushSync();
    expect(lines().some((line) => line.includes('the socket is not open'))).toBe(true);
    expect(retryButton()).not.toBeNull();
    expect(await violationsOf(document.body.innerHTML)).toEqual([]);
  });

  it('retries a held upload from its own id', async () => {
    const ids: (number | null)[] = [];
    let attempt = 0;
    const upload = vi.fn(
      (
        _seat: SessionSlot,
        _files: unknown,
        id: number | null,
        onProgress: (p: ShareProgress) => void,
      ) => {
        ids.push(id);
        attempt += 1;
        if (attempt === 1) {
          onProgress({
            kind: 'begin',
            upload_id: 42,
            attempted: true,
            resumed: false,
            received: [0],
          });
          return Promise.reject(new Error('the connection dropped'));
        }
        return Promise.resolve({ paths: ['/inbox/photo.jpg'] });
      },
    );
    draw({ upload });
    sendButton()?.click();
    await Promise.resolve();
    await Promise.resolve();
    flushSync();
    retryButton()?.click();
    await Promise.resolve();
    await Promise.resolve();
    flushSync();
    expect(ids).toEqual([null, 42]);
  });

  it('sends no prompt until the upload has answered its paths', async () => {
    let settle: (answer: { paths: string[] }) => void = () => {};
    const upload = vi.fn(
      () =>
        new Promise<{ paths: string[] }>((resolve) => {
          settle = resolve;
        }),
    );
    const { dispatched } = draw({ upload });
    sendButton()?.click();
    flushSync();
    expect(dispatched).toHaveLength(0);
    settle({ paths: ['/inbox/photo.jpg'] });
    await Promise.resolve();
    await Promise.resolve();
    flushSync();
    expect(dispatched).toHaveLength(1);
    const command = dispatched[0]?.command as Record<string, Record<string, unknown>>;
    expect(command['prompt_under']?.['text']).toBe('file: /inbox/photo.jpg');
    expect(dispatched[0]?.at).toEqual(LEAD);
  });

  it('retries only the prompt when the upload already landed', async () => {
    const upload = vi.fn(() => Promise.resolve({ paths: ['/inbox/photo.jpg'] }));
    const { connection, dispatched } = stub();
    let fail = true;
    // **A no-op until the dispatch takes it**: TypeScript cannot see an
    // assignment made inside the promise executor, so a plain callable keeps
    // the release callable without a narrowing it would reject.
    let release: () => void = () => {};
    const flaky = {
      ...connection,
      dispatch: (command: unknown, at?: SessionSlot) => {
        if (fail) {
          fail = false;
          return Promise.reject(new Error('the connection is down'));
        }
        dispatched.push(at === undefined ? { command } : { command, at });
        return new Promise<void>((resolve) => {
          release = resolve;
        });
      },
    } as unknown as Connection;
    draw({ connection: flaky, upload });
    sendButton()?.click();
    await vi.waitFor(() => {
      flushSync();
      expect(lines().some((line) => line.includes('the files are uploaded'))).toBe(true);
    });
    retryButton()?.click();
    flushSync();
    // **The landed upload is not drawn as uploading again**: only the message
    // is in flight, so the last attempt's line and bar stay gone.
    expect(lines().some((line) => line.includes('Uploading'))).toBe(false);
    expect(lines().some((line) => line.includes('Sending the message'))).toBe(true);
    release();
    await vi.waitFor(() => {
      flushSync();
      expect(lines().some((line) => line.includes('Sent to'))).toBe(true);
    });
    expect(upload).toHaveBeenCalledTimes(1);
    expect(dispatched).toHaveLength(1);
  });

  it('names the limit and sends nothing over it', () => {
    draw({
      files: [{ path: '/cache/big.bin', name: 'big.bin', size: MAX_SHARE_BYTES + 1, mime: null }],
    });
    expect(document.querySelector('.total')?.textContent).toContain('at most');
    expect(sendButton()?.disabled).toBe(true);
  });

  it('draws the reason a share carried no files', () => {
    draw({ files: [], error: 'the provider refused it' });
    expect(lines().some((line) => line.includes('the provider refused it'))).toBe(true);
    expect(sendButton()).toBeNull();
  });

  /// **A share of nothing is drawn neutrally, not as a failure.** A SEND
  /// with no stream is a share the app took and that carried no file; the
  /// sheet says that rather than accusing a share nothing went wrong with.
  it('draws a share of nothing as the neutral line', () => {
    draw({ files: [], empty: true });
    expect(lines()).toEqual(['This share carried no files.']);
    expect(document.querySelector('.line.bad')).toBeNull();
    expect(sendButton()).toBeNull();
  });

  /// **A visit to the route with no share behind it draws the same line**,
  /// which is what the book's empty state quotes.
  it('draws the neutral line on a route visit with no share', () => {
    draw({ files: [] });
    expect(lines()).toEqual(['This share carried no files.']);
  });
});

describe('the share prompt', () => {
  it('is the words, a blank line, then one file line per path', () => {
    expect(composePrompt('look at these', ['/a.jpg', '/b.pdf'])).toBe(
      'look at these\n\nfile: /a.jpg\nfile: /b.pdf',
    );
  });

  it('leaves the file lines alone when there are no words', () => {
    expect(composePrompt('   ', ['/a.jpg'])).toBe('file: /a.jpg');
  });
});

describe('a share payload', () => {
  it('narrows to the files it carries', () => {
    const shared = shareOf({
      files: [{ path: '/cache/1/a.jpg', name: 'a.jpg', size: 4, mime: 'image/jpeg' }],
      error: null,
    });
    expect(shared?.files).toEqual([
      { path: '/cache/1/a.jpg', name: 'a.jpg', size: 4, mime: 'image/jpeg' },
    ]);
    expect(shared?.error).toBeNull();
    expect(shared?.empty).toBe(false);
  });

  it('drops a row that is not a file', () => {
    const shared = shareOf({ files: [{ name: 'no path' }, { path: '/a', name: 'a' }] });
    expect(shared?.files).toHaveLength(1);
  });

  /// **A payload whose rows all failed the narrowing is a refusal, not an
  /// empty share**: the sheet draws the reason rather than the neutral line.
  it('refuses a payload whose rows are all unreadable', () => {
    const shared = shareOf({ files: [{ name: 'no path' }, { size: 3 }] });
    expect(shared?.files).toEqual([]);
    expect(shared?.error).toBe('this share named files this client cannot read');
    expect(shared?.empty).toBe(false);
  });

  it('keeps a share of nothing as its own fact', () => {
    const shared = shareOf({ files: [], error: null, empty: true });
    expect(shared?.empty).toBe(true);
  });

  it('is not a share worth opening when it names nothing', () => {
    expect(worthOpening({ files: [], error: null, empty: false })).toBe(false);
    expect(worthOpening({ files: [], error: 'refused', empty: false })).toBe(true);
    expect(worthOpening({ files: [], error: null, empty: true })).toBe(true);
    expect(shareOf({ nothing: true })).toBeNull();
  });
});

describe('a size label', () => {
  it('uses the units the limit is stated in', () => {
    expect(sizeLabel(512)).toBe('512 B');
    expect(sizeLabel(2048)).toBe('2.0 KB');
    expect(sizeLabel(5 * 1024 * 1024)).toBe('5.0 MB');
  });
});
