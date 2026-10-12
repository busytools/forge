// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { clearDrafts, docScope, draftsFor } from './comments';
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
    expect(document.querySelector('.doc-text')).toBeNull();
  });

  it('renders markdown as the chat renders prose', () => {
    draw({ kind: 'landed', document_kind: 'markdown', bytes: enc('# hi\n\ntext') });

    const prose = document.querySelector('.prose');
    expect(prose?.querySelector('h1')?.textContent).toBe('hi');
    expect(prose?.textContent).toContain('text');
  });

  it('renders plain text as text, not as markup', () => {
    draw({ kind: 'landed', document_kind: 'text', bytes: enc('<b>not bold</b>\nline') });

    const pre = document.querySelector('.doc-text');
    expect(pre?.textContent).toBe('<b>not bold</b>\nline');
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
 * The comment affordance: a reviewable document draws as its own lines,
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

function enc(text: string): Uint8Array {
  return new TextEncoder().encode(text);
}
