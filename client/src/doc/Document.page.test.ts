// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { fakeDoc, SLOT, type DocAnswer } from './testing';
import Document from './Document.svelte';

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

function enc(text: string): Uint8Array {
  return new TextEncoder().encode(text);
}
