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
    hold: () => () => {},
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
}));

import { currentRoute } from '../routes';
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
    draw({ kind: 'landed', document_kind: { other: 'rs' }, bytes: enc('fn main() {}') });

    const said = document.querySelector('.doc-said');
    expect(said?.textContent).toContain('.rs');
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
 * thing a hand-off adds, and the done is what answers it.
 */
describe('the cold open', () => {
  /** A comment the document already carries, written against this content. */
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
    buttonByText('done').click();
    await vi.waitFor(() => {
      expect(document.querySelector('.doc-left')).not.toBeNull();
    });
    const answered = (page.sent as Record<string, Record<string, unknown>>[]).find(
      (command) => 'answer_document_hand_off' in command,
    )?.['answer_document_hand_off'] as { id?: unknown } | undefined;
    expect(answered?.id, 'and the parked call the hand-off added is answered').toBe('h-1');
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
): ReturnType<typeof fakeDoc> {
  fake = fakeDoc(answer);
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
    expect(document.querySelector('button.doc-lnum'), 'no number strip').toBeNull();
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
      content_hash: '42',
      context: ['line two', 'line three'],
      base_ref: `doc:${PATH}`,
    });
    expect((thread['comments'] as { text: string }[])[0]?.text).toBe('one line, please');
    expect(document.querySelector('.doc-draft')?.textContent).toContain('drafted');
  });

  /**
   * **The done files the round and releases the hand-off.** The parked
   * call's answer carries the review the finish minted, so the agent hears
   * what the reading produced - and the review id it named is the same one
   * the submit answered with.
   */
  it('files the drafts on done and answers the hand-off with the review', async () => {
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
    buttonByText('done (1)').click();
    await vi.waitFor(() => {
      expect(document.querySelector('.doc-left')?.textContent).toContain('review left (R4)');
    });

    const submit = (page.sent as Record<string, unknown>[]).find(
      (command) => 'submit_review' in command,
    ) as { submit_review: { branch: string; thread_ids: string[] } } | undefined;
    expect(submit?.submit_review.branch).toBe(`doc:${PATH}`);
    expect(submit?.submit_review.thread_ids).toHaveLength(1);

    const answered = (page.sent as Record<string, unknown>[]).find(
      (command) => 'answer_document_hand_off' in command,
    ) as
      | { answer_document_hand_off: { id: string; answer: { done: { review_id: string } } } }
      | undefined;
    expect(answered?.answer_document_hand_off.id).toBe('h-1');
    expect(
      answered?.answer_document_hand_off.answer.done.review_id,
      'the answer carries the review the finish minted',
    ).toBe('r9');
    expect(document.querySelector('.doc-draft'), 'the drafts were filed').toBeNull();
  });

  /** **The nudge the view this client replaced sent on its own drop.** The
   *  finish pushes the review-ready line into the session, so the chat shows
   *  the review to the reader and reaches the agent - with the document's
   *  scope named. */
  it('pushes the review-ready line into the session on done', async () => {
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
    buttonByText('done (1)').click();
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

  /** **Every opened document draws its one door.** A PDF has no comments
   *  and no review, but it still parked a call: the done answers it and is
   *  the way back - the same single verb every state carries. */
  it('draws the one done on a PDF hand-off', () => {
    drawReviewable({
      kind: 'landed',
      document_kind: 'pdf',
      bytes: enc('%PDF-1.4 fake'),
      reviewable: false,
      content_hash: '42',
    });

    expect(document.querySelector('embed'), 'the PDF itself is drawn').not.toBeNull();
    expect(buttonByText('done'), 'a PDF answers the parked hand-off').toBeDefined();
    expect(
      document.querySelector('a.doc-back'),
      'and there is no second door beside it',
    ).toBeNull();
  });

  /** **The door opens even when the document never did.** A refused read
   *  has no path and no drafts; the one done still returns the reader -
   *  and answers a parked hand-off - rather than standing there inert. */
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

    buttonByText('done').click();
    await vi.waitFor(() => {
      expect(document.querySelector('.doc-left')).not.toBeNull();
    });
    const answered = (fake.sent as Record<string, Record<string, unknown>>[]).find(
      (command) => 'answer_document_hand_off' in command,
    )?.['answer_document_hand_off'] as { id?: unknown } | undefined;
    expect(answered?.id, 'the parked call is answered, not left held').toBe('h-1');
    await vi.waitFor(
      () => {
        expect(get(currentRoute)).toEqual({ name: 'session', slot: SLOT });
      },
      { timeout: 3000 },
    );
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

  it("draws a refused done in the core's own words, drafts kept", async () => {
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
    buttonByText('done (1)').click();
    await vi.waitFor(() => {
      expect(document.body.textContent).toContain('the store is not open');
    });
    expect(document.querySelector('.doc-draft'), 'nothing was cleared').not.toBeNull();

    const answered = (page.sent as Record<string, unknown>[]).find(
      (command) => 'answer_document_hand_off' in command,
    );
    expect(answered, 'a refused review answers nothing - the call stays parked').toBeUndefined();
  });

  /**
   * **The document's own hash places its comments.** One written against
   * this content sits at its line; one written against another version
   * draws outdated AT ONCE, at its captured lines - nothing re-anchors a
   * document automatically.
   */
  it('draws a thread written against another version as outdated', () => {
    const thread = {
      id: 'd9',
      anchor: {
        path: PATH,
        side: 'New',
        line: 2,
        content_hash: '41',
        context: ['^ old line', 'old three'],
        base_ref: `doc:${PATH}`,
      },
      comments: [{ author: 'User', text: 'from before', at: 'x', review_id: null }],
      status: 'Open',
      created_at: 'x',
      updated_at: 'x',
      commit: null,
    };
    const here = { ...thread, id: 'd8', anchor: { ...thread.anchor, content_hash: '42' } };
    drawReviewable({
      kind: 'landed',
      document_kind: 'markdown',
      bytes: enc('# hi\nline two\nline three'),
      reviewable: true,
      content_hash: '42',
      threads: [thread, here],
    });

    const stale = document.querySelector('.doc-stale');
    expect(stale?.textContent, 'the outdated thread reads in its own section').toContain(
      'from before',
    );
    expect(stale?.textContent, 'with its captured lines kept').toContain('^ old line');
    const inline = document.querySelectorAll('.doc-note:not(.doc-quoted)');
    expect(inline).toHaveLength(1);
    expect(inline[0]?.textContent).toContain('from before');
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
    const before = {
      id: 'd9',
      anchor: {
        path: PATH,
        side: 'New',
        line: 2,
        content_hash: '41',
        context: ['^ old line', 'old three'],
        base_ref: `doc:${PATH}`,
      },
      comments: [
        {
          author: 'User',
          text: 'a `code` span and **emphasis**\nkept on its own line',
          at: 'x',
          review_id: null,
        },
      ],
      status: 'Open',
      created_at: 'x',
      updated_at: 'x',
      commit: null,
    };
    drawReviewable({
      kind: 'landed',
      document_kind: 'markdown',
      bytes: enc('# hi\nline two\nline three'),
      reviewable: true,
      content_hash: '42',
      threads: [
        before,
        {
          ...before,
          id: 'd8',
          anchor: { ...before.anchor, content_hash: '42' },
          comments: [
            {
              author: 'User',
              text: 'a `code` span and **emphasis**\nkept on its own line',
              at: 'x',
              review_id: null,
            },
          ],
        },
      ],
    });

    const surfaces: [string, string][] = [
      ['a comment under its block', '.doc-note:not(.doc-quoted):not(.doc-draft)'],
      ['a draft waiting for the done', '.doc-draft'],
      ['a comment against another version', '.doc-quoted'],
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
    const drifted = (id: string, status: string) => ({
      id,
      anchor: {
        path: PATH,
        side: 'New',
        line: 2,
        content_hash: '41',
        context: ['^ old line'],
        base_ref: `doc:${PATH}`,
      },
      comments: [{ author: 'User', text: 'from before', at: 'x', review_id: null }],
      status,
      created_at: 'x',
      updated_at: 'x',
      commit: null,
    });
    const page = drawReviewable({
      kind: 'landed',
      document_kind: 'markdown',
      bytes: enc('# hi\nline two\nline three'),
      reviewable: true,
      content_hash: '42',
      threads: [
        drifted('d1', 'Open'),
        drifted('d2', 'Addressed'),
        drifted('d3', 'Resolved'),
        drifted('d4', 'Outdated'),
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
    compacting: false,
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
        expect(document.querySelector('.doc-compose textarea'), 'and not the field').toBeNull();
      },
      { timeout: 2000 },
    );

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
});

function enc(text: string): Uint8Array {
  return new TextEncoder().encode(text);
}
