// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { get } from 'svelte/store';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

/** The seat's read, as the page subscribes it for the take's own words: real
 *  enough to carry a record in, which is all this page takes one for. */
const seat = vi.hoisted(() => ({ listeners: new Set<(read: unknown) => void>() }));

vi.mock('../session/live', () => ({
  watchSession: () => ({
    subscribe: (fn: (read: unknown) => void) => {
      seat.listeners.add(fn);
      return () => {
        seat.listeners.delete(fn);
      };
    },
  }),
}));

/** Hand the page the record it would have folded out of a frame. */
function push(read: unknown): void {
  for (const fn of [...seat.listeners]) fn(read);
}

const mic = vi.hoisted(() => ({
  open: (): Promise<unknown> =>
    Promise.resolve({
      onFrame: null as ((bytes: Uint8Array) => void) | null,
      resolved: undefined,
      flush: () => null,
      flushWorklet: () => undefined,
      stop: () => undefined,
    }),
}));

vi.mock('../composer/mic', () => ({
  Microphone: { open: mic.open },
  inputs: vi.fn((): Promise<unknown[]> => Promise.resolve([])),
  inputLine: (): string => 'the inputs could not be listed \u{b7} try again',
  micSilence: (): Promise<null> => Promise.resolve(null),
  silenceCause: (): null => null,
}));

import { currentRoute } from '../routes';
import { anchorHash } from '../review/write';
import { addDraft, clearDrafts, docScope, draftsFor } from './comments';
import { TRUNCATED } from '../composer/view';
import { fakeDoc, SLOT, type DocAnswer } from './testing';
import Document from './Document.svelte';
import { violationsOf } from '../testing/axe';

/**
 * The viewer's states, every one drawn: the read pending, the server's
 * refusal as its own line, each kind rendered as itself, an empty document,
 * and a PDF in the surface's own viewer.
 *
 * **The missing-document case is the one a stable address meets**: a thread
 * deep-links back to a document that moved, and the page draws the refusal
 * rather than a blank.
 */
let app: Record<string, unknown> | null = null;

const PATH = '/proj/docs/plan.md';

beforeEach(() => {
  // jsdom has no blob URLs; the page makes one for a PDF's embed.
  URL.createObjectURL = vi.fn(() => 'blob:doc');
  URL.revokeObjectURL = vi.fn();
});

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  document.body.innerHTML = '';
  vi.restoreAllMocks();
  // The draft map is module-wide by design (drafts outlive the viewer);
  // each test starts from an empty scope.
  clearDrafts(
    docScope(PATH),
    draftsFor(docScope(PATH)).map((draft) => draft.id),
  );
});

function draw(answer: DocAnswer, reviewable = false): void {
  const fake = fakeDoc(answer);
  app = mount(Document, {
    target: document.body,
    props: { slot: SLOT, path: PATH, reviewable, connection: fake.connection },
  });
  flushSync();
}

const text = (): string => document.body.textContent ?? '';

describe('the document viewer', () => {
  it('draws the pending line while the read is out', () => {
    draw({ kind: 'silent' });

    expect(text()).toContain(PATH);
    // The sheet's shared pending line, which every page's own states draw.
    expect(document.querySelector('.pending')).not.toBeNull();
  });

  it("draws the server's refusal as its own line", () => {
    draw({ kind: 'refused', why: 'no document at /proj/docs/plan.md' });

    const said = document.querySelector('.doc-said');
    expect(said?.textContent).toContain('no document at /proj/docs/plan.md');
  });

  /**
   * **A text kind that is not UTF-8 is refused at the read, and the page
   * draws that refusal - never half of the file**: decoding the bytes here
   * would draw replacement characters as if they were the document.
   */
  it('draws a non-UTF-8 refusal without half-drawing anything', () => {
    draw({ kind: 'refused', why: 'the document is not UTF-8 text: /proj/docs/bad.md' });

    expect(document.querySelector('.doc-said')?.textContent).toContain('not UTF-8');
    expect(document.querySelector('.prose')).toBeNull();
    expect(document.querySelector('.doc-block'), 'and no body either').toBeNull();
  });

  it('renders markdown as the chat renders prose, never as its own marks', () => {
    draw({ kind: 'landed', document_kind: 'markdown', bytes: enc('# hi\n\ntext') });

    const heading = document.querySelector('.doc-body h1');
    expect(heading?.textContent, 'the heading is an element').toBe('hi');
    expect(document.querySelector('.doc-body')?.textContent).toContain('text');
    expect(text(), 'no raw mark reaches the page').not.toContain('# hi');
  });

  it('renders plain text as text, not as markup', () => {
    draw({ kind: 'landed', document_kind: 'text', bytes: enc('<b>not bold</b>\nline') });

    const rows = [...document.querySelectorAll('.doc-body .doc-raw')].map((row) => row.textContent);
    expect(rows).toEqual(['<b>not bold</b>', 'line']);
    expect(document.querySelector('b'), 'no markup was parsed out of it').toBeNull();
  });

  it('draws an unsupported kind as itself', () => {
    // A binary extension: a UTF-8 `.rs` reads as its own kind, and this arm
    // is about a document the viewer does not draw at all.
    draw({ kind: 'landed', document_kind: { other: 'png' }, bytes: new Uint8Array([0x89, 0x50]) });

    const said = document.querySelector('.doc-said');
    expect(said?.textContent).toContain('.png');
  });

  it('draws an empty document as its own state', () => {
    draw({ kind: 'landed', document_kind: 'markdown', bytes: new Uint8Array() });

    expect(document.querySelector('.doc-empty')).not.toBeNull();
    expect(document.querySelector('.prose'), 'nothing half-drawn').toBeNull();
  });

  /**
   * A PDF rides the surface's own viewer: an embed over a blob of the
   * bytes, which is what WKWebView and every browser render natively.
   * (The Android WebView has no PDF viewer and takes the pdf.js branch,
   * pinned in `pdf.test.ts`.)
   */
  it('embeds a PDF for the platform viewer', () => {
    draw({ kind: 'landed', document_kind: 'pdf', bytes: enc('%PDF-1.4 fake') });

    const embed = document.querySelector('embed');
    expect(embed?.getAttribute('type')).toBe('application/pdf');
    expect(embed?.getAttribute('src')).toBe('blob:doc');
  });

  it("asks for the document with the route's own seat, path and flag", () => {
    const fake = fakeDoc({ kind: 'silent' });
    app = mount(Document, {
      target: document.body,
      props: { slot: SLOT, path: PATH, reviewable: true, connection: fake.connection },
    });
    flushSync();

    expect(fake.reads).toEqual([{ slot: SLOT, path: PATH, reviewable: true }]);
  });
});

/**
 * **The door belongs to the document, not to how the page was reached.** A
 * reader the app opens cold on its last address, keeps a link to, or
 * reloads asks for the document with no hand-off behind it, and the read
 * answers what the DOCUMENT is: a readable markdown or text document is
 * reviewable, so the page draws its blocks and the comments it already
 * carries exactly as a hand-off's Open does. The parked call is the only
 * thing a hand-off adds, and it is the answer row - approve or something is
 * off - that releases it; a cold open keeps the one done.
 */
describe('the cold open', () => {
  /** A comment the document already carries, and where the read put it. */
  const held = {
    id: 'd-1',
    anchor: {
      path: PATH,
      side: 'New',
      line: 2,
      content_hash: '42',
      context: ['line two'],
      base_ref: `doc:${PATH}`,
    },
    comments: [{ author: 'User', text: 'from an earlier read', at: 'x', review_id: null }],
    status: 'Open',
    created_at: 'x',
    updated_at: 'x',
    commit: null,
    resolution: { state: 'placed', line: 2, moved_from: null },
  };

  it("draws the door and the document's own comments with no hand-off behind it", () => {
    const page = fakeDoc({
      kind: 'landed',
      document_kind: 'markdown',
      bytes: enc('# hi\nline two\nline three'),
      content_hash: '42',
      threads: [held],
    });
    app = mount(Document, {
      target: document.body,
      props: { slot: SLOT, path: PATH, reviewable: false, connection: page.connection },
    });
    flushSync();

    expect(page.reads, 'the cold open asks with the route it was handed').toEqual([
      { slot: SLOT, path: PATH, reviewable: false },
    ]);
    expect(blockTarget('line two'), 'the comment target is here with no hand-off').toBeDefined();
    expect(
      document.querySelector('.doc-note')?.textContent,
      'and the comment the document already carries is drawn',
    ).toContain('from an earlier read');
    expect(buttonByText('done'), 'with the one door every state carries').toBeDefined();
  });

  it('answers the parked call on the same door when a hand-off did raise it', async () => {
    const page = fakeDoc({
      kind: 'landed',
      document_kind: 'markdown',
      bytes: enc('# hi\nline two\nline three'),
      content_hash: '42',
      threads: [held],
    });
    app = mount(Document, {
      target: document.body,
      props: {
        slot: SLOT,
        path: PATH,
        reviewable: true,
        handoff: 'h-1',
        connection: page.connection,
      },
    });
    flushSync();

    expect(blockTarget('line two'), 'the same door a cold open draws').toBeDefined();
    buttonByText('approve').click();
    await vi.waitFor(() => {
      expect(document.querySelector('.doc-left')).not.toBeNull();
    });
    const answered = (page.sent as Record<string, Record<string, unknown>>[]).find(
      (command) => 'answer_document_hand_off' in command,
    )?.['answer_document_hand_off'] as { id?: unknown; answer?: unknown } | undefined;
    expect(answered?.id, 'and the parked call the hand-off added is answered').toBe('h-1');
    expect(answered?.answer, 'with the explicit approval, not a bare done').toEqual({
      approve: { review_id: null, note: null },
    });
  });
});

/**
 * The comment affordance: a readable document draws as its own lines,
 * each one a target; a comment anchors to the raw file's numbering and is
 * upserted the moment it is written; the finish files the drafts as one
 * review, through the same commands a code review rides.
 */
let fake: ReturnType<typeof fakeDoc> | null = null;

function drawReviewable(
  answer: Parameters<typeof fakeDoc>[0] = {
    kind: 'landed',
    document_kind: 'markdown',
    bytes: enc('# hi\nline two\nline three'),
    reviewable: true,
    content_hash: '42',
  },
  handoff: string | null = 'h-1',
): ReturnType<typeof fakeDoc> {
  fake = fakeDoc(answer);
  app = mount(Document, {
    target: document.body,
    props: {
      slot: SLOT,
      path: PATH,
      reviewable: true,
      handoff,
      connection: fake.connection,
    },
  });
  flushSync();
  return fake;
}

/** The comment target whose rendered text contains `text`. */
function blockTarget(text: string): HTMLElement {
  const found = [...document.querySelectorAll('.doc-block')].find((block) =>
    block.textContent?.includes(text),
  );
  if (!(found instanceof HTMLElement)) throw new Error(`no block reads "${text}"`);
  return found;
}

function buttonByText(text: string): HTMLButtonElement {
  const found = [...document.querySelectorAll('button')].find(
    (button) => button.textContent?.trim() === text,
  );
  if (!(found instanceof HTMLButtonElement)) throw new Error(`no button reads "${text}"`);
  return found;
}

describe('the reviewable document', () => {
  it('renders the document as its blocks, each one a comment target', () => {
    drawReviewable();

    const blocks = document.querySelectorAll('.doc-block');
    expect(blocks.length, 'a heading, a paragraph - one row each').toBeGreaterThanOrEqual(2);
    expect(blocks[0]?.querySelector('h1'), 'the markdown rendered, not raw').not.toBeNull();
    expect(
      blocks[0]?.getAttribute('aria-label'),
      'the row itself is the target, named for what it does',
    ).toBe('comment on this part of the document');
    expect(
      [...blocks].every((block) => block.classList.contains('prose')),
      'each block carries the chat prose rules, so its marks draw as the chat draws them',
    ).toBe(true);
    // The hover target: one `+` per block, aligned with the block's own
    // source line and named for it.
    const adds = [...document.querySelectorAll<HTMLButtonElement>('button.doc-add')];
    expect(adds.map((add) => add.getAttribute('aria-label'))).toEqual([
      'comment on line 1',
      'comment on line 2',
    ]);
    // The always-visible source-line numbers are gone.
    expect(document.querySelectorAll('button.doc-lnum')).toHaveLength(0);
    expect(document.querySelectorAll('.doc-lines')).toHaveLength(0);
  });

  /**
   * **A list is one target per item.** A markdown chunk that draws ten
   * bullets is ten things a reader may comment on, so each item renders on
   * its own line and anchors at its own source line.
   */
  it('draws one target per list item, each anchored at its own line', () => {
    const page = drawReviewable({
      kind: 'landed',
      document_kind: 'markdown',
      bytes: enc('# hi\n\n- one\n- two\n- three\n'),
      reviewable: true,
      content_hash: '42',
    });

    const items = [...document.querySelectorAll<HTMLElement>('.doc-block')].filter(
      (block) => block.querySelector('.doc-block-body > ul') !== null,
    );
    expect(
      items.map((item) => item.querySelector('.doc-block-body')?.textContent?.trim()),
      'one row per bullet',
    ).toEqual(['one', 'two', 'three']);
    expect(
      items.map((item) => item.querySelector('.doc-add')?.getAttribute('aria-label')),
      'each item carries its own line target',
    ).toEqual(['comment on line 3', 'comment on line 4', 'comment on line 5']);

    items[1]?.click();
    flushSync();
    const composer = document.querySelector<HTMLTextAreaElement>('.doc-compose textarea');
    if (composer === null) throw new Error('no composer');
    composer.value = 'on the second bullet';
    composer.dispatchEvent(new Event('input', { bubbles: true }));
    flushSync();
    buttonByText('comment').click();
    flushSync();

    const sent = (page.sent as Record<string, Record<string, Record<string, unknown>>>[])[0];
    const anchor = sent?.['upsert_review_thread']?.['thread'] as
      { anchor?: { line?: number } } | undefined;
    expect(anchor?.anchor?.line, 'the second bullet anchors at line 4').toBe(4);
  });

  /** **A quote's paragraphs are targets too**, not one target for the whole
   *  quote. */
  it('splits a quote into one target per paragraph', () => {
    drawReviewable({
      kind: 'landed',
      document_kind: 'markdown',
      bytes: enc('> one\n>\n> two\n'),
      reviewable: true,
      content_hash: '42',
    });

    const quotes = [...document.querySelectorAll('.doc-block')].filter(
      (block) => block.querySelector('.doc-block-body > blockquote') !== null,
    );
    expect(
      quotes.map((quote) => quote.querySelector('.doc-block-body')?.textContent?.trim()),
    ).toEqual(['one', 'two']);
    expect(
      quotes.map((quote) => quote.querySelector('.doc-add')?.getAttribute('aria-label')),
    ).toEqual(['comment on line 1', 'comment on line 3']);
  });

  /** **A leaf inside a quote is a target of its own.** A fence opens
   *  nothing, so it must not be run through the closer search - a quote
   *  ending in a fence would drop it and draw the raw markers. */
  it('draws a fenced code block at the end of a quote', () => {
    drawReviewable({
      kind: 'landed',
      document_kind: 'markdown',
      bytes: enc('> intro\n>\n> ```\n> const x = 1;\n> ```\n'),
      reviewable: true,
      content_hash: '42',
    });

    const quotes = [...document.querySelectorAll('.doc-block')].filter(
      (block) => block.querySelector('.doc-block-body > blockquote') !== null,
    );
    expect(quotes).toHaveLength(2);
    const code = quotes[1]?.querySelector('.doc-block-body .code pre');
    expect(code?.textContent, 'the fence is rendered as code').toContain('const x = 1;');
    expect(
      document.querySelector('.doc-body')?.textContent,
      'no raw markdown markers reach the page',
    ).not.toContain('```');
    expect(
      quotes[1]?.querySelector('.doc-add')?.getAttribute('aria-label'),
      'and the fence keeps its own line',
    ).toBe('comment on line 3');
  });

  /** **A quote's fence does not swallow the paragraph after it.** The leaf
   *  is one target and the paragraph another, each anchored at its own line. */
  it('keeps a quoted fence and its following paragraph as separate targets', () => {
    drawReviewable({
      kind: 'landed',
      document_kind: 'markdown',
      bytes: enc('> ```\n> const x = 1;\n> ```\n>\n> after the code\n'),
      reviewable: true,
      content_hash: '42',
    });

    const quotes = [...document.querySelectorAll('.doc-block')].filter(
      (block) => block.querySelector('.doc-block-body > blockquote') !== null,
    );
    expect(quotes).toHaveLength(2);
    const code = quotes[0]?.querySelector('.doc-block-body .code pre');
    expect(code?.textContent, 'the fence renders first').toContain('const x = 1;');
    const after = quotes[1]?.querySelector('.doc-block-body');
    expect(after?.textContent?.trim(), 'the paragraph is its own target').toBe('after the code');
    expect(
      quotes[1]?.querySelector('.doc-add')?.getAttribute('aria-label'),
      'and anchors at its own line',
    ).toBe('comment on line 5');
  });

  /**
   * **The hover target and the block are two anchors.** A press on the
   * rendered block is a comment on the section; a press on its `+` is a
   * comment on the block's own source line, the line first in its captured
   * context.
   */
  it('opens the composer at a block and at its line', () => {
    drawReviewable();

    blockTarget('line two').click();
    flushSync();
    expect(
      document.querySelector('.doc-compose textarea')?.getAttribute('aria-label'),
      'the block press anchors at the section',
    ).toBe('comment on this part of the document');
    buttonByText('cancel').click();
    flushSync();

    const line = blockTarget('line two').querySelector<HTMLButtonElement>('button.doc-add');
    expect(line, "the block's own line carries the `+`").not.toBeNull();
    line?.click();
    flushSync();
    expect(
      document.querySelector('.doc-compose textarea')?.getAttribute('aria-label'),
      "the `+` anchors at the block's line",
    ).toBe('comment on line 2');
  });

  /**
   * **One feedback box.** The answer's note and the round's description are
   * the same words in the same box - no separate summary field beside it -
   * and both the submit and the answer carry them.
   */
  it('draws one feedback box for the note and the round description', async () => {
    const page = drawReviewable();

    blockTarget('line two').click();
    flushSync();
    const composer = document.querySelector('textarea');
    if (!(composer instanceof HTMLTextAreaElement)) throw new Error('no composer');
    composer.value = 'hello';
    composer.dispatchEvent(new Event('input', { bubbles: true }));
    flushSync();
    buttonByText('comment').click();
    flushSync();

    expect(
      document.querySelectorAll('textarea[placeholder^="feedback on the document"]').length,
      'exactly one feedback box',
    ).toBe(1);
    expect(
      document.querySelector('textarea[placeholder="summary (optional)"]'),
      'no separate summary box',
    ).toBeNull();
    expect(
      document.querySelector('textarea[placeholder="note (optional)"]'),
      'no separate note box',
    ).toBeNull();
    const box = document.querySelector<HTMLTextAreaElement>(
      'textarea[placeholder="feedback on the document..."]',
    );
    expect(box, 'the one box is the feedback box').not.toBeNull();
    if (box === null) return;
    box.value = 'the one: the second paragraph';
    box.dispatchEvent(new Event('input', { bubbles: true }));
    flushSync();

    page.submitAnswer = { body: { id: 'r9', number: 4, summary: null, created_at: 'x' } };
    buttonByText('approve').click();
    await vi.waitFor(() => {
      expect(document.querySelector('.doc-left')?.textContent).toContain('approved');
    });

    const submit = (page.sent as Record<string, unknown>[]).find(
      (command) => 'submit_review' in command,
    ) as { submit_review: { summary: string | null } } | undefined;
    expect(submit?.submit_review.summary, 'the box describes the round').toBe(
      'the one: the second paragraph',
    );
    const answered = (page.sent as Record<string, unknown>[]).find(
      (command) => 'answer_document_hand_off' in command,
    )?.['answer_document_hand_off'] as
      { answer?: { approve?: { note: string | null } } } | undefined;
    expect(answered?.answer?.approve?.note, 'and the note rides the answer').toBe(
      'the one: the second paragraph',
    );
  });

  /**
   * **The second answer is "send feedback", and it needs words.** The
   * requirement is said in the row rather than by disabling the button: a
   * disabled control leaves the tab order and explains nothing, so an empty
   * press draws the requirement and sends nothing.
   */
  it('names the second answer send feedback and says what it needs', () => {
    const page = drawReviewable();

    expect(
      [...document.querySelectorAll('button')].some(
        (button) => button.textContent?.trim() === 'something is off',
      ),
      'the old wording is gone',
    ).toBe(false);
    const send = buttonByText('send feedback');
    expect(send.disabled, 'the button stays in the tab order').toBe(false);
    expect(
      document.querySelector('.doc-exit .doc-hint')?.textContent,
      'the requirement is visible before the press',
    ).toContain('send feedback needs words');

    send.click();
    flushSync();
    expect(
      document.querySelector('.doc-hint.warn')?.textContent,
      'an empty press draws the requirement',
    ).toContain('write what is off');
    expect(
      (page.sent as Record<string, unknown>[]).some(
        (command) => 'answer_document_hand_off' in command,
      ),
      'and answers nothing',
    ).toBe(false);

    const box = document.querySelector<HTMLTextAreaElement>(
      'textarea[placeholder="feedback on the document..."]',
    );
    if (box === null) throw new Error('no feedback box');
    box.value = 'the second paragraph is stale';
    box.dispatchEvent(new Event('input', { bubbles: true }));
    flushSync();

    expect(document.querySelector('.doc-hint.warn'), 'words clear the warning').toBeNull();
    expect(buttonByText('approve').disabled, 'approve never needed them').toBe(false);
  });

  /**
   * **The reader resolves a thread; the agent never does.** Every open
   * thread carries a resolve control beside its delete, and the write is the
   * same status command every surface's resolve rides.
   */
  it('resolves an open thread, writing Resolved', () => {
    const page = drawReviewable({
      kind: 'landed',
      document_kind: 'markdown',
      bytes: enc('# hi\nline two\nline three'),
      reviewable: true,
      content_hash: '42',
      threads: [
        {
          ...placedThread('d9', 2, { state: 'placed', line: 2, moved_from: null }, ['line two']),
          comments: [
            { author: 'User', text: 'from before', at: 'x', review_id: null },
            {
              author: { Agent: { label: 'implementer' } },
              text: 'addressed it',
              at: 'x',
              review_id: null,
            },
          ],
        },
      ],
    });

    const resolve = [...document.querySelectorAll<HTMLButtonElement>('.doc-note .rv-a')].find(
      (button) => button.textContent?.trim() === 'resolve',
    );
    expect(resolve, 'an open thread draws the resolve act').toBeDefined();
    resolve?.click();
    flushSync();

    const status = (page.sent as Record<string, Record<string, unknown>>[])
      .filter((command) => 'set_review_thread_status' in command)
      .map((command) => command['set_review_thread_status']);
    expect(status).toEqual([
      { project: SLOT.project, branch: `doc:${PATH}`, thread_id: 'd9', status: 'Resolved' },
    ]);
    expect(
      document.querySelector('.doc-note')?.textContent,
      "the agent's reply stays a reply",
    ).toContain('addressed it');
  });

  it('comments on a block: the upsert carries the block anchor', () => {
    const page = drawReviewable();

    blockTarget('line two').click();
    flushSync();
    const box = document.querySelector('textarea');
    if (!(box instanceof HTMLTextAreaElement)) throw new Error('no composer');
    box.value = 'one line, please';
    box.dispatchEvent(new Event('input', { bubbles: true }));
    flushSync();
    buttonByText('comment').click();
    flushSync();

    const [sent] = page.sent as Record<string, Record<string, Record<string, unknown>>>[];
    const thread = sent?.['upsert_review_thread']?.['thread'] as Record<string, unknown>;
    expect(sent?.['upsert_review_thread']?.['branch']).toBe(`doc:${PATH}`);
    expect(thread['commit']).toBeNull();
    expect(thread['anchor']).toEqual({
      path: PATH,
      side: 'New',
      line: 2,
      content_hash: anchorHash('line two').toString(),
      context: ['line two', 'line three'],
      base_ref: `doc:${PATH}`,
    });
    expect((thread['comments'] as { text: string }[])[0]?.text).toBe('one line, please');
    expect(document.querySelector('.doc-draft')?.textContent).toContain('drafted');
  });

  /**
   * **A press on a block's `+` comments on THAT line**, with the line first
   * in its captured context - the line a revision re-finds the thread by.
   * The block's own press still captures the whole section, so both kinds
   * of comment are one store shape with a different anchor.
   */
  it('comments on a line: the upsert anchors at the line, line first', () => {
    const page = drawReviewable({
      kind: 'landed',
      document_kind: 'markdown',
      // The paragraph's own line is 3, after the heading and the blank.
      bytes: enc('# one\n\ntwo\nthree\n'),
      reviewable: true,
      content_hash: '42',
    });

    const second = blockTarget('two').querySelector<HTMLButtonElement>('button.doc-add');
    expect(second, "the block draws its line's hover target").toBeDefined();
    second?.click();
    flushSync();

    const box = document.querySelector('textarea');
    if (!(box instanceof HTMLTextAreaElement)) throw new Error('no composer');
    box.value = 'on this line';
    box.dispatchEvent(new Event('input', { bubbles: true }));
    flushSync();
    buttonByText('comment').click();
    flushSync();

    const [sent] = page.sent as Record<string, Record<string, Record<string, unknown>>>[];
    const thread = sent?.['upsert_review_thread']?.['thread'] as Record<string, unknown>;
    expect(thread['anchor']).toEqual({
      path: PATH,
      side: 'New',
      line: 3,
      content_hash: anchorHash('two').toString(),
      context: ['two', 'three'],
      base_ref: `doc:${PATH}`,
    });
    // The draft is drawn under the block that owns the line, named for the
    // line it will file against.
    const draft = document.querySelector('.doc-draft');
    expect(draft?.textContent, 'the draft draws').toContain('on this line');
    expect(draft?.textContent, 'and says which line it is on').toContain('line 3');
  });

  /** **Cancel closes the box.** A line composer cancelled at the block's
   *  own press would reopen as a section comment at line 1, so cancel is a
   *  close, not a press. */
  it('closes a line composer on cancel', () => {
    const page = drawReviewable({
      kind: 'landed',
      document_kind: 'markdown',
      bytes: enc('one\ntwo\nthree\n'),
      reviewable: true,
      content_hash: '42',
    });

    blockTarget('two').querySelector<HTMLButtonElement>('button.doc-add')?.click();
    flushSync();
    const box = document.querySelector('textarea');
    if (!(box instanceof HTMLTextAreaElement)) throw new Error('no composer');
    box.value = 'never mind';
    box.dispatchEvent(new Event('input', { bubbles: true }));
    flushSync();

    buttonByText('cancel').click();
    flushSync();

    expect(document.querySelector('.doc-compose'), 'the box closed').toBeNull();
    expect(page.sent, 'and nothing was written').toHaveLength(0);
  });

  /**
   * **Done files the round and releases the hand-off.** The parked call's
   * answer carries the review the finish minted, so the agent hears what
   * the reading produced - and the review id it named is the same one the
   * submit answered with.
   */
  it('files the drafts on approve and answers the hand-off with the review', async () => {
    const page = drawReviewable();

    blockTarget('line two').click();
    flushSync();
    const box = document.querySelector('textarea');
    if (!(box instanceof HTMLTextAreaElement)) throw new Error('no composer');
    box.value = 'hello';
    box.dispatchEvent(new Event('input', { bubbles: true }));
    flushSync();
    buttonByText('comment').click();
    flushSync();

    // The one box above the door is the round's description and the answer's
    // note, so the same words ride both.
    const feedback = document.querySelector<HTMLTextAreaElement>(
      'textarea[placeholder="feedback on the document..."]',
    );
    if (feedback === null) throw new Error('no feedback box');
    feedback.value = 'the doc reads well, one note';
    feedback.dispatchEvent(new Event('input', { bubbles: true }));
    flushSync();

    page.submitAnswer = { body: { id: 'r9', number: 4, summary: null, created_at: 'x' } };
    buttonByText('approve').click();
    await vi.waitFor(() => {
      expect(document.querySelector('.doc-left')?.textContent).toContain('review left (R4)');
    });

    const submit = (page.sent as Record<string, unknown>[]).find(
      (command) => 'submit_review' in command,
    ) as
      | { submit_review: { branch: string; thread_ids: string[]; summary: string | null } }
      | undefined;
    expect(submit?.submit_review.branch).toBe(`doc:${PATH}`);
    expect(submit?.submit_review.thread_ids).toHaveLength(1);
    expect(submit?.submit_review.summary, 'the description is the round summary').toBe(
      'the doc reads well, one note',
    );

    const answered = (page.sent as Record<string, unknown>[]).find(
      (command) => 'answer_document_hand_off' in command,
    ) as
      | {
          answer_document_hand_off: {
            id: string;
            answer: { approve: { review_id: string; note: string | null } };
          };
        }
      | undefined;
    expect(answered?.answer_document_hand_off.id).toBe('h-1');
    expect(
      answered?.answer_document_hand_off.answer.approve.review_id,
      'the answer carries the review the finish minted',
    ).toBe('r9');
    expect(answered?.answer_document_hand_off.answer.approve.note, 'the box is the note too').toBe(
      'the doc reads well, one note',
    );
    expect(document.querySelector('.doc-left')?.textContent, 'approved, not merely done').toContain(
      'approved',
    );
    expect(document.querySelector('.doc-draft'), 'the drafts were filed').toBeNull();
  });

  /**
   * **A comment written here is editable and retractable, like every other
   * surface's** (Ved, 2026-10-17): the draft's edit opens the page's own box
   * over its text and save rewrites the stored thread, and delete takes the
   * draft off the shelf and out of the store.
   */
  it("edits and retracts a draft's words", () => {
    const page = drawReviewable();

    blockTarget('line two').click();
    flushSync();
    const box = document.querySelector('textarea');
    if (!(box instanceof HTMLTextAreaElement)) throw new Error('no composer');
    box.value = 'the first words';
    box.dispatchEvent(new Event('input', { bubbles: true }));
    flushSync();
    buttonByText('comment').click();
    flushSync();

    const edit = [...document.querySelectorAll<HTMLButtonElement>('.doc-draft .rv-a')].find(
      (button) => button.textContent?.trim() === 'edit',
    );
    expect(edit, 'a draft draws the edit act').toBeDefined();
    edit?.click();
    flushSync();

    const field = document.querySelector<HTMLTextAreaElement>('.doc-editbox .rv-txarea');
    expect(field, 'the box opens over the draft').not.toBeNull();
    expect(field?.value, "and it holds the draft's words").toBe('the first words');
    if (field !== null) {
      field.value = 'the better words';
      field.dispatchEvent(new Event('input', { bubbles: true }));
      flushSync();
    }
    [...document.querySelectorAll<HTMLButtonElement>('.doc-editbox .rv-acts button')]
      .find((button) => button.textContent?.trim() === 'save')
      ?.click();
    flushSync();

    // The LAST upsert: writing the comment was the first, and the edit is
    // the one this press made.
    const upsert = (page.sent as Record<string, unknown>[])
      .filter((command) => 'upsert_review_thread' in command)
      .at(-1) as { upsert_review_thread: { thread: { comments: { text: string }[] } } } | undefined;
    expect(upsert?.upsert_review_thread.thread.comments[0]?.text, 'save rewrites the store').toBe(
      'the better words',
    );
    expect(
      document.querySelector('.doc-draft')?.textContent,
      'and the draft row reads the new words',
    ).toContain('the better words');

    const del = [...document.querySelectorAll<HTMLButtonElement>('.doc-draft .rv-a')].find(
      (button) => button.textContent?.trim() === 'delete',
    );
    del?.click();
    flushSync();
    const go = [...document.querySelectorAll<HTMLButtonElement>('.rv-delask .rv-a')].find(
      (button) => button.textContent?.trim() === 'delete',
    );
    expect(go, 'the retraction asks first').toBeDefined();
    go?.click();
    flushSync();

    expect(document.querySelector('.doc-draft'), 'the draft went with the confirm').toBeNull();
    const cut = (page.sent as Record<string, unknown>[]).find(
      (command) => 'delete_review_thread' in command,
    ) as { delete_review_thread: { thread_id: string } } | undefined;
    expect(cut?.delete_review_thread.thread_id, 'and the store heard about it').toBeTruthy();
  });

  /**
   * **A written comment draws once.** The store echoes the draft back on the
   * next read, so the thread it answers with carries the draft's own id; the
   * shelf is the truth while the comment is unfiled, and the echo must not
   * draw a second row beside it. The first and last lines are the anchors a
   * boundary slip would duplicate first, so both are carried here.
   */
  it('draws a written comment once when the store echoes it back', () => {
    addDraft(docScope(PATH), { id: 't-1', line: 1, text: 'the first words', context: ['# hi'] });
    addDraft(docScope(PATH), {
      id: 't-3',
      line: 3,
      text: 'the last words',
      context: ['line three'],
    });
    const first = {
      ...placedThread('t-1', 1, { state: 'placed', line: 1, moved_from: null }, ['# hi']),
      comments: [{ author: 'User', text: 'the first words', at: 'x', review_id: null }],
    };
    const last = {
      ...placedThread('t-3', 3, { state: 'placed', line: 2, moved_from: 3 }, ['line three']),
      comments: [{ author: 'User', text: 'the last words', at: 'x', review_id: null }],
    };
    drawReviewable({
      kind: 'landed',
      document_kind: 'markdown',
      // The last line's text now sits at line 2, so the echo's move is the
      // placement the server's re-anchor would produce.
      bytes: enc('# hi\nline three\nline two'),
      reviewable: true,
      content_hash: '42',
      threads: [first, last],
    });

    const rows = [...document.querySelectorAll('.doc-note')];
    expect(rows.length, 'no store echo draws beside either draft').toBe(2);
    expect(
      rows.every((row) => row.classList.contains('doc-draft')),
      'both rows are the shelf rows',
    ).toBe(true);
    const words = rows.map((row) => row.textContent ?? '');
    expect(
      words.some((text) => text.includes('the first words')),
      'the line 1 draft draws',
    ).toBe(true);
    expect(
      words.some((text) => text.includes('the last words')),
      'the last-line draft draws',
    ).toBe(true);
  });

  /**
   * **The page ages the echoes it draws, never one a shelf row covers.** A
   * suppressed echo's stale landing must not reach the store behind the
   * reader's back; every other drifted thread still ages, including a draft
   * whose own line is past the document and can only draw as its echo.
   */
  it('ages every drifted echo except one its draft row covers', () => {
    addDraft(docScope(PATH), {
      id: 't-1',
      line: 2,
      text: 'the words',
      context: ['line two'],
    });
    addDraft(docScope(PATH), {
      id: 't-5',
      line: 5,
      text: 'past the document',
      context: ['line two'],
    });
    const suppressed = {
      ...placedThread('t-1', 2, { state: 'outdated', reason: 'gone' }, ['line two']),
      comments: [{ author: 'User', text: 'the words', at: 'x', review_id: null }],
    };
    const drifted = placedThread('d-9', 2, { state: 'outdated', reason: 'gone' }, ['line two']);
    const past = {
      ...placedThread('t-5', 5, { state: 'outdated', reason: 'gone' }, ['line two']),
      comments: [{ author: 'User', text: 'past the document', at: 'x', review_id: null }],
    };
    const page = drawReviewable({
      kind: 'landed',
      document_kind: 'markdown',
      bytes: enc('# hi\nline two\nline three'),
      reviewable: true,
      content_hash: '42',
      threads: [suppressed, drifted, past],
    });

    const aged = (page.sent as Record<string, Record<string, unknown>>[])
      .filter((command) => 'set_review_thread_status' in command)
      .map((command) => command['set_review_thread_status']);
    expect(aged, 'only the echoes the page draws are aged').toEqual([
      { project: SLOT.project, branch: `doc:${PATH}`, thread_id: 'd-9', status: 'Outdated' },
      { project: SLOT.project, branch: `doc:${PATH}`, thread_id: 't-5', status: 'Outdated' },
    ]);
  });

  /**
   * **A draft past this version's last line is the store's row to draw.**
   * The draft row only draws inside the document's rows, so a comment
   * written at line 5 of a document now three lines long has no shelf row;
   * the store re-found its content at line 2, and dropping that echo would
   * leave the comment with no row at all while the finish still files it.
   */
  it('draws the stored thread when a draft line is past the document', () => {
    addDraft(docScope(PATH), {
      id: 't-2',
      line: 5,
      text: 'the moved words',
      context: ['line two'],
    });
    const echo = {
      ...placedThread('t-2', 5, { state: 'placed', line: 2, moved_from: 5 }, ['line two']),
      comments: [{ author: 'User', text: 'the moved words', at: 'x', review_id: null }],
    };
    drawReviewable({
      kind: 'landed',
      document_kind: 'markdown',
      bytes: enc('# hi\nline two\nline three'),
      reviewable: true,
      content_hash: '42',
      threads: [echo],
    });

    const rows = [...document.querySelectorAll('.doc-note')].filter((row) =>
      row.textContent?.includes('the moved words'),
    );
    expect(rows.length, 'the stored thread is the only row').toBe(1);
    expect(rows[0]?.classList.contains('doc-draft'), 'the shelf has no row to draw').toBe(false);
  });

  /** **The nudge the view this client replaced sent on its own drop.** The
   *  finish pushes the review-ready line into the session, so the chat shows
   *  the review to the reader and reaches the agent - with the document's
   *  scope named. */
  it('pushes the review-ready line into the session on the answer', async () => {
    const page = drawReviewable();

    blockTarget('line two').click();
    flushSync();
    const box = document.querySelector('textarea');
    if (!(box instanceof HTMLTextAreaElement)) throw new Error('no composer');
    box.value = 'hello';
    box.dispatchEvent(new Event('input', { bubbles: true }));
    flushSync();
    buttonByText('comment').click();
    flushSync();

    page.submitAnswer = { body: { id: 'r9', number: 4, summary: null, created_at: 'x' } };
    buttonByText('approve').click();
    await vi.waitFor(() => {
      expect(document.querySelector('.doc-left')?.textContent).toContain('review left (R4)');
    });

    const nudge = (page.sent as Record<string, Record<string, unknown>>[]).find(
      (command) => 'prompt_under' in command,
    )?.['prompt_under'] as { text?: unknown } | undefined;
    expect(nudge?.text, 'the line names the review and the document scope').toContain(
      'Review #4 ready on the document',
    );
    expect(nudge?.text).toContain(`doc:${PATH}`);
    expect(nudge?.text, 'and names no review tool the asking seat may lack').not.toContain(
      'review__',
    );
  });

  /** **A reload of the address mounts the viewer with no hand-off**, and
   *  the read still answers reviewable, so a review can be filed with no
   *  parked call behind it. The line must stand alone: nothing that follows
   *  will tell the agent how to read the review back. */
  it('files a review with no hand-off and sends a self-contained line', async () => {
    const page = drawReviewable(undefined, null);

    blockTarget('line two').click();
    flushSync();
    const box = document.querySelector('textarea');
    if (!(box instanceof HTMLTextAreaElement)) throw new Error('no composer');
    box.value = 'hello';
    box.dispatchEvent(new Event('input', { bubbles: true }));
    flushSync();
    buttonByText('comment').click();
    flushSync();

    page.submitAnswer = { body: { id: 'r9', number: 4, summary: null, created_at: 'x' } };
    buttonByText('done (1)').click();
    await vi.waitFor(() => {
      expect(document.querySelector('.doc-left')?.textContent).toContain('review left (R4)');
    });

    const nudge = (page.sent as Record<string, Record<string, unknown>>[]).find(
      (command) => 'prompt_under' in command,
    )?.['prompt_under'] as { text?: unknown } | undefined;
    expect(nudge?.text, 'the line names the review and the scope').toContain(
      'Review #4 ready on the document',
    );
    expect(nudge?.text).toContain(`doc:${PATH}`);
    expect(nudge?.text).toContain('Read it back under that scope');
    expect(nudge?.text, 'and hands the scope to the lead when it lacks the tools').toContain(
      'or hand the scope to your lead',
    );
    expect(nudge?.text, 'and promises no hand-off answer that never comes').not.toContain(
      'hand-off answer',
    );
    const answered = (page.sent as Record<string, Record<string, unknown>>[]).find(
      (command) => 'answer_document_hand_off' in command,
    );
    expect(answered, 'no parked call exists to answer').toBeUndefined();
  });

  /** **A link inside the block is the link's.** A click landing on an anchor
   *  navigates; it must not also open the composer under the navigation. */
  it('leaves a link inside a block alone', () => {
    drawReviewable({
      kind: 'landed',
      document_kind: 'markdown',
      bytes: enc('# hi\n\nsee [the plan](./plan.md) for more\n'),
      reviewable: true,
      content_hash: '42',
    });

    const link = document.querySelector('.doc-block a');
    expect(link, 'the markdown link rendered').not.toBeNull();
    link?.dispatchEvent(new MouseEvent('click', { bubbles: true }));
    flushSync();
    expect(
      document.querySelector('.doc-compose'),
      'the block did not open its composer over the link',
    ).toBeNull();
  });

  /** **The keyboard half of the link rule.** A reader tabbed onto a link
   *  inside a block is on the link: Enter must reach it (not be swallowed
   *  by the block) and open no composer - while Enter ON the block still
   *  opens its composer, which is the block's own affordance. */
  it('gives a focused link its Enter, and the block its own', () => {
    drawReviewable({
      kind: 'landed',
      document_kind: 'markdown',
      bytes: enc('# hi\n\nsee [the plan](./plan.md) for more\n'),
      reviewable: true,
      content_hash: '42',
    });

    const link = document.querySelector('.doc-block a');
    expect(link, 'the markdown link rendered').not.toBeNull();
    const onLink = new KeyboardEvent('keydown', {
      key: 'Enter',
      bubbles: true,
      cancelable: true,
    });
    link?.dispatchEvent(onLink);
    flushSync();
    expect(onLink.defaultPrevented, "the link's Enter was not swallowed").toBe(false);
    expect(
      document.querySelector('.doc-compose'),
      'and the block did not open its composer over the link',
    ).toBeNull();

    const block = blockTarget('see');
    const onBlock = new KeyboardEvent('keydown', { key: 'Enter', bubbles: true, cancelable: true });
    block.dispatchEvent(onBlock);
    flushSync();
    expect(onBlock.defaultPrevented, 'the block still answers its own Enter').toBe(true);
    expect(document.querySelector('.doc-compose'), 'and opens its composer').not.toBeNull();
  });

  /** **A PDF has no comments to write, but it still parked a call**: the two
   *  answers draw and each carries the optional note - the same door every
   *  state carries, with the PDF opening read-only above it. */
  it('draws both answers on a PDF hand-off', () => {
    drawReviewable({
      kind: 'landed',
      document_kind: 'pdf',
      bytes: enc('%PDF-1.4 fake'),
      reviewable: false,
      content_hash: '42',
    });

    expect(document.querySelector('embed'), 'the PDF itself is drawn').not.toBeNull();
    expect(buttonByText('approve'), 'a PDF answers the parked hand-off').toBeDefined();
    expect(buttonByText('send feedback'), 'with the explicit other answer').toBeDefined();
    expect(
      document.querySelector('textarea[placeholder="feedback on the document..."]'),
      'and the one feedback box rides with either',
    ).not.toBeNull();
    expect(
      document.querySelector('button.doc-comment-btn'),
      'read-only: no comment affordance',
    ).toBeNull();
  });

  /** **The door opens even when the document never did.** A refused read
   *  has no path and no drafts; the two answers still return the reader -
   *  and release the parked hand-off - rather than standing there inert. */
  it('returns from a refused read, answering the parked hand-off', async () => {
    const fake = fakeDoc({ kind: 'refused', why: 'no document at /proj/docs/gone.md' });
    app = mount(Document, {
      target: document.body,
      props: {
        slot: SLOT,
        path: PATH,
        reviewable: true,
        handoff: 'h-1',
        connection: fake.connection,
      },
    });
    flushSync();
    expect(document.querySelector('.doc-said')?.textContent, 'the refusal is drawn').toContain(
      'no document at',
    );

    const box = document.querySelector<HTMLTextAreaElement>(
      'textarea[placeholder="feedback on the document..."]',
    );
    if (box === null) throw new Error('no feedback box');
    box.value = 'the document is gone, re-hand it';
    box.dispatchEvent(new Event('input', { bubbles: true }));
    flushSync();

    buttonByText('send feedback').click();
    await vi.waitFor(() => {
      expect(document.querySelector('.doc-left')).not.toBeNull();
    });
    const answered = (fake.sent as Record<string, Record<string, unknown>>[]).find(
      (command) => 'answer_document_hand_off' in command,
    )?.['answer_document_hand_off'] as { id?: unknown; answer?: unknown } | undefined;
    expect(answered?.id, 'the parked call is answered, not left held').toBe('h-1');
    expect(answered?.answer, 'with the explicit feedback ending').toEqual({
      feedback: { review_id: null, note: 'the document is gone, re-hand it' },
    });
    await vi.waitFor(
      () => {
        expect(get(currentRoute)).toEqual({ name: 'session', slot: SLOT });
      },
      { timeout: 3000 },
    );
  });

  /** **The feedback box draws only when it has a job.** A parked call always
   *  carries it as the answer note; a deep link has no call, so the box is
   *  the review's description and appears only once a draft needs one. */
  it('draws the feedback box on a deep link only when there is a review to describe', () => {
    const fake = fakeDoc({
      kind: 'landed',
      document_kind: 'markdown',
      bytes: enc('# hi\nline two\nline three'),
      content_hash: '42',
    });
    app = mount(Document, {
      target: document.body,
      props: { slot: SLOT, path: PATH, reviewable: false, connection: fake.connection },
    });
    flushSync();
    expect(
      document.querySelector('textarea[placeholder^="feedback on the document"]'),
      'a bare deep link has no box',
    ).toBeNull();

    blockTarget('line two').click();
    flushSync();
    const composer = document.querySelector('textarea');
    if (!(composer instanceof HTMLTextAreaElement)) throw new Error('no composer');
    composer.value = 'hello';
    composer.dispatchEvent(new Event('input', { bubbles: true }));
    flushSync();
    buttonByText('comment').click();
    flushSync();

    expect(
      document.querySelector('textarea[placeholder^="feedback on the document"]'),
      'a draft gives the description a box',
    ).not.toBeNull();
    expect(
      document.querySelector('.doc-hint'),
      'a deep link has no send feedback to require words',
    ).toBeNull();
  });

  /** Nothing to release (a deep link): the one door is still the way back,
   *  answering nothing and filing nothing. */
  it('returns from a bare viewer without dispatching anything', async () => {
    const fake = fakeDoc({
      kind: 'landed',
      document_kind: 'markdown',
      bytes: enc('# hi'),
      reviewable: false,
      content_hash: '42',
    });
    app = mount(Document, {
      target: document.body,
      props: { slot: SLOT, path: PATH, reviewable: false, connection: fake.connection },
    });
    flushSync();

    buttonByText('done').click();
    await vi.waitFor(
      () => {
        expect(get(currentRoute)).toEqual({ name: 'session', slot: SLOT });
      },
      { timeout: 3000 },
    );
    expect(fake.sent, 'a viewer with nothing to release sends nothing').toEqual([]);
  });

  it("draws a refused answer in the core's own words, drafts kept", async () => {
    const page = drawReviewable();

    blockTarget('line two').click();
    flushSync();
    const box = document.querySelector('textarea');
    if (!(box instanceof HTMLTextAreaElement)) throw new Error('no composer');
    box.value = 'hello';
    box.dispatchEvent(new Event('input', { bubbles: true }));
    flushSync();
    buttonByText('comment').click();
    flushSync();

    page.submitAnswer = { reject: 'the store is not open' };
    buttonByText('approve').click();
    await vi.waitFor(() => {
      expect(document.body.textContent).toContain('the store is not open');
    });
    expect(document.querySelector('.doc-draft'), 'nothing was cleared').not.toBeNull();

    const answered = (page.sent as Record<string, unknown>[]).find(
      (command) => 'answer_document_hand_off' in command,
    );
    expect(answered, 'a refused review answers nothing - the call stays parked').toBeUndefined();
  });

  /** One thread with a landing, for the revision tests below. */
  function placedThread(
    id: string,
    line: number,
    resolution: Record<string, unknown> | null,
    context: string[] = ['line two'],
  ): Record<string, unknown> {
    return {
      id,
      anchor: {
        path: PATH,
        side: 'New',
        line,
        content_hash: anchorHash(context[0] ?? '').toString(),
        context,
        base_ref: `doc:${PATH}`,
      },
      comments: [{ author: 'User', text: 'from before', at: 'x', review_id: null }],
      status: 'Open',
      created_at: 'x',
      updated_at: 'x',
      commit: null,
      resolution,
    };
  }

  const revision = (threads: Record<string, unknown>[]) =>
    drawReviewable({
      kind: 'landed',
      document_kind: 'markdown',
      bytes: enc('# hi\nline two\nline three'),
      reviewable: true,
      content_hash: '42',
      threads,
    });

  /**
   * **A thread the landing moved draws at its new line, not at the bottom.**
   * The revision re-found its first captured line, and the old line is named
   * beside it so the reader can see what moved.
   */
  it('keeps a moved thread inline at the line its content moved to', () => {
    // The anchor was the heading block on line 1; the landing is the
    // paragraph block on line 3, so the thread has to draw under a DIFFERENT
    // row than the one it was written on.
    const moved = placedThread('d9', 1, { state: 'placed', line: 3, moved_from: 1 }, ['line one']);
    moved['comments'] = [
      { author: 'User', text: 'from before', at: 'x', review_id: null },
      {
        author: { Agent: { label: 'implementer' } },
        text: 'addressed it',
        at: 'x',
        review_id: null,
      },
    ];
    revision([moved]);

    const note = document.querySelector('.doc-note:not(.doc-quoted)');
    expect(note?.textContent, 'the thread draws under its block').toContain('from before');
    expect(note?.textContent, 'and names where it came from').toContain('moved from 1');
    expect(note?.textContent, "the agent's reply reads beside it").toContain('addressed it');
    // **The ROW is what places it**: the anchor was the heading block on line
    // 1, so a thread drawn by `anchor.line` would sit after `# hi`, not after
    // the paragraph whose line 3 carried the landing.
    expect(
      blockTarget('line three').nextElementSibling?.classList.contains('doc-note'),
      'the thread is the next sibling of the block that holds its landing',
    ).toBe(true);
    expect(document.querySelector('.doc-stale'), 'nothing collects at the bottom').toBeNull();
  });

  /**
   * **A thread the revision left nowhere still draws inline, marked.** Its
   * captured line is the reader's, and collecting it below the document
   * would hide what it was about; the mark says the content is gone.
   */
  it('keeps a thread with no landing inline, marked outdated', () => {
    revision([placedThread('d9', 2, { state: 'outdated', reason: 'gone' }, ['^ old line'])]);

    const note = document.querySelector('.doc-note:not(.doc-quoted)');
    expect(note?.textContent, 'the thread stays where it was written').toContain('from before');
    expect(note?.textContent, 'marked for what happened to it').toContain('outdated');
    expect(document.querySelector('.doc-stale'), 'still not the bottom section').toBeNull();
  });

  /** **Only a captured line the document no longer reaches has no place**:
   *  that thread reads in the bottom section, its captured lines kept and
   *  the reason said in words. */
  it('collects a thread whose captured line is past the document', () => {
    revision([placedThread('d9', 42, { state: 'outdated', reason: 'gone' }, ['^ old line'])]);

    const stale = document.querySelector('.doc-stale');
    expect(stale?.textContent, 'the thread reads in the bottom section').toContain('from before');
    expect(stale?.textContent, 'with its captured lines kept').toContain('^ old line');
    expect(stale?.textContent, 'and the reason said in words').toContain(
      'line 42 is past this version',
    );
    expect(document.querySelector('.doc-note:not(.doc-quoted)'), 'nothing draws inline').toBeNull();
  });

  /** **A thread with no place is still the reader's to resolve.** The
   *  bottom section draws the same act as the inline cards. */
  it('resolves a thread with no place in this version', () => {
    const page = revision([
      placedThread('d9', 42, { state: 'outdated', reason: 'gone' }, ['^ old line']),
    ]);

    const resolve = [...document.querySelectorAll<HTMLButtonElement>('.doc-quoted .rv-a')].find(
      (button) => button.textContent?.trim() === 'resolve',
    );
    expect(resolve, 'the bottom section draws the resolve act').toBeDefined();
    resolve?.click();
    flushSync();

    const status = (page.sent as Record<string, Record<string, unknown>>[])
      .filter((command) => 'set_review_thread_status' in command)
      .map((command) => command['set_review_thread_status']);
    // The drift ages the thread to Outdated on the way in; the press is the
    // write this test is about.
    expect(status.at(-1)).toEqual({
      project: SLOT.project,
      branch: `doc:${PATH}`,
      thread_id: 'd9',
      status: 'Resolved',
    });
  });

  /** **A resolved thread with no place keeps a reopen**: the bottom section
   *  must not be a one-way door. */
  it('reopens a resolved thread with no place in this version', () => {
    const page = revision([
      {
        ...placedThread('d9', 42, { state: 'outdated', reason: 'gone' }, ['^ old line']),
        status: 'Resolved',
      },
    ]);

    const reopen = [...document.querySelectorAll<HTMLButtonElement>('.doc-quoted .rv-a')].find(
      (button) => button.textContent?.trim() === 'reopen',
    );
    expect(reopen, 'a resolved bottom-section thread draws reopen').toBeDefined();
    reopen?.click();
    flushSync();

    const wrote = (page.sent as Record<string, Record<string, unknown>>[])
      .filter((command) => 'set_review_thread_status' in command)
      .map((command) => command['set_review_thread_status']);
    expect(wrote).toEqual([
      { project: SLOT.project, branch: `doc:${PATH}`, thread_id: 'd9', status: 'Open' },
    ]);
  });

  /** **The blank lines before the first block are rows too.** A thread placed
   *  on one draws inline; with no row for the gap it was drawn nowhere. */
  it('draws a thread placed on the blank line before the first block', () => {
    drawReviewable({
      kind: 'landed',
      document_kind: 'markdown',
      bytes: enc('\n# hi\n'),
      reviewable: true,
      content_hash: '42',
      threads: [placedThread('d9', 1, { state: 'placed', line: 1, moved_from: null }, [''])],
    });

    const note = document.querySelector('.doc-note:not(.doc-quoted)');
    expect(note?.textContent, 'the thread draws under the lead row').toContain('from before');
    expect(document.querySelector('.doc-stale'), 'and not at the bottom').toBeNull();
  });

  /** **A read with no landing is an older server, not drift.** Nothing ages
   *  when the server never placed the thread; otherwise a new client beside
   *  an older server writes Outdated into the store for every thread. */
  it('does not age a thread the read gave no landing', () => {
    const page = drawReviewable({
      kind: 'landed',
      document_kind: 'markdown',
      bytes: enc('# hi\nline two\nline three'),
      reviewable: true,
      content_hash: '42',
      threads: [placedThread('d9', 2, null, ['line two'])],
    });

    const aged = (page.sent as Record<string, Record<string, unknown>>[]).filter(
      (command) => 'set_review_thread_status' in command,
    );
    expect(aged).toEqual([]);
  });

  /**
   * **The comment words render as the chat does.** A comment is markdown
   * wherever it is read: the review page's turns go through the chat's own
   * `Prose` with the author's lines kept, and the viewer's notes read the
   * same - a backtick or an emphasis in a comment arrives as the element,
   * never as the character. Every note surface is checked: a comment placed
   * under its block, a draft waiting for the done, and a comment written
   * against another version.
   */
  it('renders comment words as the chat does, marks and lines alike', () => {
    addDraft(docScope(PATH), {
      id: 'dr-1',
      line: 2,
      text: 'a `code` span and **emphasis**\nkept on its own line',
      context: ['line two'],
    });
    const words = 'a `code` span and **emphasis**\nkept on its own line';
    const before = {
      ...placedThread('d9', 42, { state: 'outdated', reason: 'gone' }, ['^ old line', 'old three']),
      comments: [{ author: 'User', text: words, at: 'x', review_id: null }],
    };
    const here = {
      ...placedThread('d8', 2, { state: 'placed', line: 2, moved_from: null }, ['line two']),
      comments: [{ author: 'User', text: words, at: 'x', review_id: null }],
    };
    drawReviewable({
      kind: 'landed',
      document_kind: 'markdown',
      bytes: enc('# hi\nline two\nline three'),
      reviewable: true,
      content_hash: '42',
      threads: [before, here],
    });

    const surfaces: [string, string][] = [
      ['a comment under its block', '.doc-note:not(.doc-quoted):not(.doc-draft)'],
      ['a draft waiting for the answer', '.doc-draft'],
      ['a comment with no place at all', '.doc-quoted'],
    ];
    for (const [where, selector] of surfaces) {
      const words = document.querySelector(`${selector} .prose`);
      expect(words?.querySelector('code')?.textContent, `${where}: the code mark`).toBe('code');
      expect(words?.querySelector('strong')?.textContent, `${where}: the emphasis`).toBe(
        'emphasis',
      );
      expect(words?.querySelector('br'), `${where}: the author's newline is kept`).not.toBeNull();
      expect(
        words?.textContent ?? '',
        `${where}: no mark reaches the page as a character`,
      ).not.toContain('`');
    }
  });

  /**
   * **The drift is written back where it is drawn.** A diff's overlay ages
   * its drifted threads in the store as it re-anchors them; a document has
   * no overlay, so the viewer is the view that does it - the review record
   * must read what the reader sees. Resolved and already-outdated comments
   * stay as they are, the same guard the overlay keeps.
   */
  it('ages a drifted thread in the store while drawing it', () => {
    const drifted = (id: string, status: string, resolution: Record<string, unknown>) => ({
      ...placedThread(id, 2, resolution, ['^ old line']),
      status,
    });
    const page = drawReviewable({
      kind: 'landed',
      document_kind: 'markdown',
      bytes: enc('# hi\nline two\nline three'),
      reviewable: true,
      content_hash: '42',
      threads: [
        drifted('d1', 'Open', { state: 'outdated', reason: 'gone' }),
        drifted('d2', 'Addressed', { state: 'outdated', reason: 'gone' }),
        drifted('d3', 'Resolved', { state: 'placed', line: 2, moved_from: null }),
        drifted('d4', 'Outdated', { state: 'outdated', reason: 'gone' }),
      ],
    });

    const aged = (page.sent as Record<string, Record<string, unknown>>[])
      .filter((command) => 'set_review_thread_status' in command)
      .map((command) => command['set_review_thread_status']);
    expect(aged).toEqual([
      { project: SLOT.project, branch: `doc:${PATH}`, thread_id: 'd1', status: 'Outdated' },
      { project: SLOT.project, branch: `doc:${PATH}`, thread_id: 'd2', status: 'Outdated' },
    ]);
  });

  /** The rendered document with a composer open, through axe: every block
   *  target carries its own name, and the field is labelled. */
  it('draws the comment affordance with no axe violations', async () => {
    drawReviewable();
    blockTarget('line two').click();
    flushSync();

    const found = await violationsOf(document.body.innerHTML);

    expect(found.map((violation) => violation.id)).toEqual([]);
  });

  /** A second hand-off moves the viewer; the first document's unsent
   *  comments wait in their scope. This is case 5's draft half, at the
   *  viewer's own level. */
  it('keeps a draft when the viewer leaves the document and comes back', async () => {
    const page = drawReviewable();
    blockTarget('line three').click();
    flushSync();
    const box = document.querySelector('textarea');
    if (!(box instanceof HTMLTextAreaElement)) throw new Error('no composer');
    box.value = 'unsent';
    box.dispatchEvent(new Event('input', { bubbles: true }));
    flushSync();
    buttonByText('comment').click();
    flushSync();
    expect(page.sent).toHaveLength(1);

    // Away and back: the route rebuilds the page, and the scope's drafts
    // are the module map's, not the component's.
    if (app !== null) await unmount(app);
    app = null;
    document.body.innerHTML = '';
    drawReviewable();

    expect(document.querySelector('.doc-draft')?.textContent).toContain('unsent');
  });
});

/**
 * The two answers a parked hand-off takes: approve, or send feedback.
 * Each draws idle, sending and its refusal in place, and the one feedback
 * box reaches the agent verbatim with whichever one is chosen. Neither
 * leaves the call answered until the review has landed.
 */
describe('the hand-off answers', () => {
  const feedbackBox = (): HTMLTextAreaElement => {
    const found = document.querySelector<HTMLTextAreaElement>(
      'textarea[placeholder="feedback on the document..."]',
    );
    if (found === null) throw new Error('no feedback box');
    return found;
  };

  it('sends feedback with its words verbatim and draws sending, then sent back', async () => {
    const page = drawReviewable();

    expect(feedbackBox().getAttribute('aria-label'), 'the feedback box is named').toBe(
      'feedback on the document',
    );
    feedbackBox().value = 'the second paragraph is stale';
    feedbackBox().dispatchEvent(new Event('input', { bubbles: true }));
    flushSync();

    buttonByText('send feedback').click();
    flushSync();
    expect(
      buttonByText('sending...'),
      'the press is drawn while the answer is in flight',
    ).toBeDefined();

    await vi.waitFor(() => {
      expect(document.querySelector('.doc-left')?.textContent).toContain('sent back');
    });
    const answered = (page.sent as Record<string, Record<string, unknown>>[]).find(
      (command) => 'answer_document_hand_off' in command,
    )?.['answer_document_hand_off'] as
      { answer?: { feedback?: { review_id: string | null; note: string } } } | undefined;
    expect(answered?.answer?.feedback).toEqual({
      review_id: null,
      note: 'the second paragraph is stale',
    });
  });

  /** **Approval carries its note too**, and the review with it. */
  it('sends approval with its note verbatim and the review', async () => {
    const page = drawReviewable();

    blockTarget('line two').click();
    flushSync();
    const box = document.querySelector('textarea');
    if (!(box instanceof HTMLTextAreaElement)) throw new Error('no composer');
    box.value = 'a note to file';
    box.dispatchEvent(new Event('input', { bubbles: true }));
    flushSync();
    buttonByText('comment').click();
    flushSync();

    feedbackBox().value = 'the measure is clear now';
    feedbackBox().dispatchEvent(new Event('input', { bubbles: true }));
    flushSync();

    page.submitAnswer = { body: { id: 'r9', number: 4, summary: null, created_at: 'x' } };
    buttonByText('approve').click();
    await vi.waitFor(() => {
      expect(document.querySelector('.doc-left')?.textContent).toContain('approved');
    });
    const answered = (page.sent as Record<string, Record<string, unknown>>[]).find(
      (command) => 'answer_document_hand_off' in command,
    )?.['answer_document_hand_off'] as
      { answer?: { approve?: { review_id: string | null; note: string } } } | undefined;
    expect(answered?.answer?.approve).toEqual({
      review_id: 'r9',
      note: 'the measure is clear now',
    });
  });

  /** **A refused answer keeps the review it already filed.** The drafts are
   *  gone with the filing, so a retry must answer with the same review id
   *  rather than telling the agent a review exists with no id. */
  it('keeps the filed review id across a refused answer', async () => {
    const page = drawReviewable();

    blockTarget('line two').click();
    flushSync();
    const box = document.querySelector('textarea');
    if (!(box instanceof HTMLTextAreaElement)) throw new Error('no composer');
    box.value = 'a note to file';
    box.dispatchEvent(new Event('input', { bubbles: true }));
    flushSync();
    buttonByText('comment').click();
    flushSync();

    page.submitAnswer = { body: { id: 'r9', number: 4, summary: null, created_at: 'x' } };
    page.answerReject = 'the call is gone';
    buttonByText('approve').click();
    await vi.waitFor(() => {
      expect(document.querySelector('.doc-said')?.textContent).toContain('the call is gone');
    });

    buttonByText('approve').click();
    await vi.waitFor(() => {
      expect(document.querySelector('.doc-left')?.textContent).toContain('approved');
    });
    const answers = (page.sent as Record<string, Record<string, unknown>>[])
      .filter((command) => 'answer_document_hand_off' in command)
      .map(
        (command) =>
          (
            command['answer_document_hand_off'] as {
              answer: { approve: { review_id: string | null } };
            }
          ).answer.approve.review_id,
      );
    expect(answers, 'both presses name the review the first one filed').toEqual(['r9', 'r9']);
  });

  /** **A stale answer is refused where it was pressed.** The answer command
   *  is fire-and-forget, so the socket's own error frame is what says the
   *  call was gone; the page must draw it and stay rather than leave as
   *  though the answer had landed. */
  it('draws a socket-refused answer and stays on the viewer', async () => {
    // The route is the harness's marker: the page only ever moves it on a
    // successful answer, so a stale 'session' left by an earlier test is not
    // this page's doing.
    currentRoute.set({ name: 'doc', slot: SLOT, path: PATH, reviewable: true });
    const page = drawReviewable();
    page.answerRefusal = 'that document hand-off is no longer waiting';

    buttonByText('approve').click();
    await vi.waitFor(() => {
      expect(document.querySelector('.doc-said')?.textContent).toContain('no longer waiting');
    });
    await new Promise((resolve) => setTimeout(resolve, 1400));

    expect(get(currentRoute), 'the viewer stays where the refusal was drawn').toMatchObject({
      name: 'doc',
    });
  });

  /** **No outcome chosen is no answer**: writing a comment files nothing
   *  and releases nothing, and the two answers stay the only doors out. */
  it('leaves the parked call held when no answer is chosen', () => {
    const page = drawReviewable();

    blockTarget('line two').click();
    flushSync();
    const box = document.querySelector('textarea');
    if (!(box instanceof HTMLTextAreaElement)) throw new Error('no composer');
    box.value = 'still thinking';
    box.dispatchEvent(new Event('input', { bubbles: true }));
    flushSync();
    buttonByText('comment').click();
    flushSync();

    const answered = (page.sent as Record<string, Record<string, unknown>>[]).find(
      (command) => 'answer_document_hand_off' in command,
    );
    expect(answered, 'the call is still parked').toBeUndefined();
    expect(buttonByText('approve'), 'both answers are still drawn').toBeDefined();
    expect(buttonByText('send feedback')).toBeDefined();
  });

  /** **The pressed button says what it is doing**, and a refusal draws where
   *  the buttons are, with both able to be pressed again. */
  it("draws a refused answer's words in place and leaves both buttons live", async () => {
    const page = drawReviewable();

    blockTarget('line two').click();
    flushSync();
    const box = document.querySelector('textarea');
    if (!(box instanceof HTMLTextAreaElement)) throw new Error('no composer');
    box.value = 'a note to file';
    box.dispatchEvent(new Event('input', { bubbles: true }));
    flushSync();
    buttonByText('comment').click();
    flushSync();

    feedbackBox().value = 'a word to send';
    feedbackBox().dispatchEvent(new Event('input', { bubbles: true }));
    flushSync();

    page.submitAnswer = { reject: 'the store is not open' };
    buttonByText('approve').click();
    flushSync();
    expect(buttonByText('approving...'), 'the approval is drawn in flight').toBeDefined();

    await vi.waitFor(() => {
      expect(document.querySelector('.doc-said')?.textContent).toContain('the store is not open');
    });
    expect(buttonByText('approve').disabled, 'approve is live again').toBe(false);
    expect(buttonByText('send feedback').disabled, 'and so is the other answer').toBe(false);
  });
});

/**
 * **The block's box dictates the way the composer's does.** A landed take's
 * words go into the box that was open when it settled, and the take's bar
 * takes that box's own line while it runs - the same bargain the review page
 * strikes, over this page's one box.
 */
describe("the block's own take", () => {
  /** The seat's record, of which this page reads the composer alone. */
  function seatRecord(composer: unknown): unknown {
    return { wire: { composer }, refused: null };
  }

  const IDLE = {
    take: null,
    notice: null,
    machine: null,
    signIn: null,
    bind: 'right_cmd',
    mode: 'auto',
  };
  const TAKE = {
    ...IDLE,
    take: {
      phase: 'recording',
      levels: [],
      peakDb: -50,
      progress: { done: 0, total: null },
      elapsedMs: 900,
      floorDb: -50,
    },
  };
  const LANDED = {
    ...IDLE,
    notice: { kind: 'landed', text: 'the second line reads well', truncated: false, generation: 1 },
  };

  /** The record while the server transcribes a take the page let go. */
  const TRANSCRIBING = { ...TAKE, take: { ...TAKE.take, phase: 'transcribing' } };

  /** The viewer with the dictation offer on, which every mic is gated by. */
  function drawWithDictation(): ReturnType<typeof fakeDoc> {
    const page = fakeDoc({
      kind: 'landed',
      document_kind: 'markdown',
      bytes: enc('# hi\nline two\nline three'),
      reviewable: true,
      content_hash: '42',
    });
    app = mount(Document, {
      target: document.body,
      props: {
        slot: SLOT,
        path: PATH,
        reviewable: true,
        handoff: 'h-1',
        connection: page.connection,
        dictation: true,
      },
    });
    flushSync();
    return page;
  }

  function composeMic(): HTMLButtonElement {
    const found = document.querySelector<HTMLButtonElement>('.doc-compose .mic');
    if (found === null) throw new Error("the block's box drew no mic");
    return found;
  }

  it('lands the words in the box that was open, with the bar taking its line', async () => {
    const page = drawWithDictation();
    push(seatRecord(IDLE));
    blockTarget('line two').click();
    flushSync();

    composeMic().click();
    await vi.waitFor(
      () => {
        expect(
          document.querySelector('.doc-compose .takeline'),
          "the take's bar, in the box's own line",
        ).not.toBeNull();
        expect(document.querySelector('.doc-compose textarea'), 'and not the field').toBeNull();
      },
      { timeout: 2000 },
    );
    const start = (page.sent as Record<string, Record<string, unknown>>[]).find(
      (command) => command['dictate_stream'] !== undefined,
    );
    expect(
      start?.['dictate_stream'],
      'the take names this page, so the record hands its words back here',
    ).toMatchObject({ surface: 'doc' });

    // The server registers the take where the record can see it, then the
    // release lands the words - the order the frames really arrive in.
    push(seatRecord(TAKE));
    flushSync();
    push(seatRecord(LANDED));
    flushSync();

    // The reader's release: the take ends and the field comes back with the
    // words in it.
    composeMic().click();
    await vi.waitFor(
      () => {
        expect(document.querySelector('.doc-compose .takeline'), 'the bar goes').toBeNull();
      },
      { timeout: 2000 },
    );
    expect(
      document.querySelector<HTMLTextAreaElement>('.doc-compose textarea')?.value,
      'the words, at the end of the box that was open',
    ).toBe('the second line reads well');
  });

  /** **The feedback box takes dictation too.** Its mic is the box's own,
   *  and the words land there and ride the answer the reader sends. */
  it('lands the words in the feedback box and carries them with the answer', async () => {
    const page = drawWithDictation();
    push(seatRecord(IDLE));

    const feedbackMic = (): HTMLButtonElement => {
      const found = document.querySelector<HTMLButtonElement>('.doc-exit .mic');
      if (found === null) throw new Error('the feedback box drew no mic');
      return found;
    };
    feedbackMic().click();
    await vi.waitFor(
      () => {
        expect(document.querySelector('.doc-exit .takeline'), "the take's bar").not.toBeNull();
      },
      { timeout: 2000 },
    );
    push(seatRecord(TAKE));
    flushSync();
    push(seatRecord(LANDED));
    flushSync();
    feedbackMic().click();
    await vi.waitFor(
      () => {
        expect(document.querySelector('.doc-exit .takeline'), 'the bar goes').toBeNull();
      },
      { timeout: 2000 },
    );

    const feedback = document.querySelector<HTMLTextAreaElement>(
      'textarea[placeholder="feedback on the document..."]',
    );
    expect(feedback?.value, 'the words land in the feedback box').toBe(
      'the second line reads well',
    );

    buttonByText('approve').click();
    await vi.waitFor(() => {
      expect(document.querySelector('.doc-left')?.textContent).toContain('approved');
    });
    const answered = (page.sent as Record<string, Record<string, unknown>>[]).find(
      (command) => 'answer_document_hand_off' in command,
    )?.['answer_document_hand_off'] as
      { answer?: { approve?: { note: string | null } } } | undefined;
    expect(answered?.answer?.approve?.note, 'and the note rides the answer').toBe(
      'the second line reads well',
    );
  });

  /**
   * **The seat's carried words are the composer's to place, not this page's**
   * (#1360), even after this page has had a take of its own: the guard reads
   * whether a take is in flight, not the last box the page began - a stale
   * box must not be re-opened for words that are the composer's, nor draw the
   * truncated warn row about them.
   */
  it('draws nothing for a carried notice after this page has had a take', async () => {
    drawWithDictation();
    push(seatRecord(IDLE));
    blockTarget('line two').click();
    flushSync();
    composeMic().click();
    await vi.waitFor(
      () => {
        expect(document.querySelector('.doc-compose .takeline'), "the take's bar").not.toBeNull();
      },
      { timeout: 2000 },
    );
    push(seatRecord(TAKE));
    flushSync();
    push(seatRecord(LANDED));
    flushSync();
    composeMic().click();
    await vi.waitFor(
      () => {
        expect(document.querySelector('.doc-compose .takeline'), 'the bar goes').toBeNull();
      },
      { timeout: 2000 },
    );

    push(
      seatRecord({
        ...IDLE,
        notice: {
          kind: 'landed',
          text: 'the unsent words',
          truncated: true,
          carried: true,
          generation: 4,
        },
      }),
    );
    flushSync();

    expect(
      document.body.textContent,
      'no warn row about words this page cannot hold',
    ).not.toContain(TRUNCATED);
    expect(
      document.querySelector<HTMLTextAreaElement>('.doc-compose textarea')?.value ?? '',
      'and the stale box was not opened with words the composer owns',
    ).not.toContain('the unsent words');
  });

  /**
   * **A quick take the record never showed still lands.** The record a page
   * reads is written once per painted frame, so a press and a release can fold
   * into one write with no take in it at all: the arm is the press's own, and
   * the words land in the box that began the take all the same (the composer's
   * own quick-release case, Ved, 2026-10-15).
   */
  it('lands a take the record never showed, after a quick release', async () => {
    drawWithDictation();
    push(seatRecord(IDLE));
    blockTarget('line two').click();
    flushSync();

    composeMic().click();
    await vi.waitFor(
      () => {
        expect(
          document.querySelector('.doc-compose .takeline'),
          "the take's bar, in the box's own line",
        ).not.toBeNull();
      },
      { timeout: 2000 },
    );

    // The words land with the record never having shown a take: only the
    // press knows this take exists on the page's side.
    push(seatRecord(LANDED));
    flushSync();

    // The reader's release: the take ends and the field comes back with the
    // words in it.
    composeMic().click();
    await vi.waitFor(
      () => {
        expect(document.querySelector('.doc-compose .takeline'), 'the bar goes').toBeNull();
      },
      { timeout: 2000 },
    );
    expect(
      document.querySelector<HTMLTextAreaElement>('.doc-compose textarea')?.value,
      'the words of a take the record never showed',
    ).toBe('the second line reads well');
  });

  /**
   * **A cancelled take leaves no arm behind.** Closing the box ends the take
   * and produces no notice: an arm left standing would ride the next landing
   * on the seat - words this page's own take never produced (the gate's F3,
   * 2026-10-15).
   */
  it('places nothing from a landing after its take was cancelled', async () => {
    drawWithDictation();
    push(seatRecord(IDLE));
    blockTarget('line two').click();
    flushSync();

    composeMic().click();
    await vi.waitFor(
      () => {
        expect(
          document.querySelector('.doc-compose .takeline'),
          "the take's bar, in the box's own line",
        ).not.toBeNull();
      },
      { timeout: 2000 },
    );

    // The reader closes the box: the take is cancelled, and no notice is
    // coming for it.
    const cancel = [
      ...document.querySelectorAll<HTMLButtonElement>('.doc-compose-acts button'),
    ].find((button) => button.textContent?.trim() === 'cancel');
    if (cancel === undefined) throw new Error("the block's box drew no cancel");
    cancel.click();
    flushSync();

    // A landing arrives on the seat all the same, and nothing places it: the
    // box stays closed, and no text of those words is anywhere.
    push(seatRecord(LANDED));
    flushSync();

    expect(
      document.querySelector<HTMLTextAreaElement>('.doc-compose textarea')?.value ?? '',
      'words from a landing whose take was cancelled',
    ).toBe('');
  });

  /**
   * **A take goes with its box.** Closing the composer mid-take ends the
   * take: without it the microphone keeps capturing into a box nothing
   * draws, and the words land where the reader cannot see them.
   */
  it('ends the take when its box closes', async () => {
    const page = drawWithDictation();
    push(seatRecord(IDLE));
    blockTarget('line two').click();
    flushSync();

    composeMic().click();
    await vi.waitFor(
      () => {
        expect(
          document.querySelector('.doc-compose .takeline'),
          "the take's bar, in the box's own line",
        ).not.toBeNull();
      },
      { timeout: 2000 },
    );
    push(seatRecord(TAKE));
    flushSync();

    const cancel = [
      ...document.querySelectorAll<HTMLButtonElement>('.doc-compose-acts button'),
    ].find((button) => button.textContent?.trim() === 'cancel');
    if (cancel === undefined) throw new Error("the block's box drew no cancel");
    page.sent.length = 0;
    cancel.click();
    flushSync();

    expect(page.sent, 'a take outlived the box that closed').toEqual([
      { dictate_stop: { key: SLOT, submit: false } },
    ]);
  });

  /**
   * **A submitted take's words are never dropped silently here either.** The
   * reader finishes in the block's box, closes it while the record still
   * transcribes, and the words land afterwards: the box is opened again at
   * the row that began the take rather than the words going nowhere.
   */
  it('opens the box again when the words land after it closed', async () => {
    drawWithDictation();
    push(seatRecord(IDLE));
    blockTarget('line two').click();
    flushSync();

    composeMic().click();
    await vi.waitFor(
      () => {
        expect(
          document.querySelector('.doc-compose .takeline'),
          "the take's bar, in the box's own line",
        ).not.toBeNull();
      },
      { timeout: 2000 },
    );
    push(seatRecord(TAKE));
    flushSync();
    composeMic().click();
    push(seatRecord(TRANSCRIBING));
    flushSync();

    const cancel = [
      ...document.querySelectorAll<HTMLButtonElement>('.doc-compose-acts button'),
    ].find((button) => button.textContent?.trim() === 'cancel');
    if (cancel === undefined) throw new Error("the block's box drew no cancel");
    cancel.click();
    flushSync();

    push(seatRecord(LANDED));
    flushSync();
    await vi.waitFor(
      () => {
        expect(
          document.querySelector<HTMLTextAreaElement>('.doc-compose textarea')?.value,
          'the words landed after the box closed and went nowhere the reader can see',
        ).toBe('the second line reads well');
      },
      { timeout: 2000 },
    );
  });

  /** **A press on another line is another box.** The take goes with the
   *  exact target it began on, not merely the row: the hover target is live
   *  while it runs, and words aimed at line 2 must not land in line 1's
   *  composer. */
  it('cancels the take when the reader moves to another line mid-take', async () => {
    drawWithDictation();
    push(seatRecord(IDLE));

    const add = (line: string): HTMLButtonElement => {
      const found = [...document.querySelectorAll<HTMLButtonElement>('button.doc-add')].find(
        (button) => button.getAttribute('aria-label') === `comment on line ${line}`,
      );
      if (found === undefined) throw new Error(`no hover target for line ${line}`);
      return found;
    };

    add('2').click();
    flushSync();
    composeMic().click();
    await vi.waitFor(
      () => {
        expect(document.querySelector('.doc-compose .takeline'), "the take's bar").not.toBeNull();
      },
      { timeout: 2000 },
    );
    push(seatRecord(TAKE));
    flushSync();

    add('1').click();
    flushSync();
    // The server ends the cancelled take, as it does on the wire; the bar is
    // the record's, so the record is what says the take is over.
    push(seatRecord(IDLE));
    flushSync();

    expect(
      document.querySelector('.doc-compose .takeline'),
      'the take ended with the box it belonged to',
    ).toBeNull();
    expect(
      document
        .querySelector<HTMLTextAreaElement>('.doc-compose textarea')
        ?.getAttribute('aria-label'),
      'and the box is the line the reader pressed',
    ).toBe('comment on line 1');

    push(seatRecord(LANDED));
    flushSync();
    expect(
      document.querySelector<HTMLTextAreaElement>('.doc-compose textarea')?.value ?? '',
      'the cancelled take put no words in the new box',
    ).toBe('');
  });

  /** **A line composer reopens on its own line after a take.** The rows share
   *  one block here, so a reopen that kept only the row would come back as a
   *  section comment at line 2's block start, not at line 2's number. */
  it('reopens a line composer on the line it began, not the block', async () => {
    drawWithDictation();
    push(seatRecord(IDLE));

    blockTarget('line two').querySelector<HTMLButtonElement>('button.doc-add')?.click();
    flushSync();

    composeMic().click();
    await vi.waitFor(
      () => {
        expect(document.querySelector('.doc-compose .takeline'), "the take's bar").not.toBeNull();
      },
      { timeout: 2000 },
    );
    push(seatRecord(TAKE));
    flushSync();
    composeMic().click();
    push(seatRecord(TRANSCRIBING));
    flushSync();

    const cancel = [
      ...document.querySelectorAll<HTMLButtonElement>('.doc-compose-acts button'),
    ].find((button) => button.textContent?.trim() === 'cancel');
    if (cancel === undefined) throw new Error("the line's box drew no cancel");
    cancel.click();
    flushSync();

    push(seatRecord(LANDED));
    flushSync();
    await vi.waitFor(
      () => {
        const field = document.querySelector<HTMLTextAreaElement>('.doc-compose textarea');
        expect(field?.getAttribute('aria-label'), 'the box reopens on its own line').toBe(
          'comment on line 2',
        );
        expect(field?.value, 'with the words in it').toBe('the second line reads well');
      },
      { timeout: 2000 },
    );
  });
});

function enc(text: string): Uint8Array {
  return new TextEncoder().encode(text);
}
