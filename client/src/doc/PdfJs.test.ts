// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

const { getDocument } = vi.hoisted(() => ({ getDocument: vi.fn() }));
vi.mock('pdfjs-dist', () => ({ getDocument, GlobalWorkerOptions: {} }));

import PdfJs from './PdfJs.svelte';

/**
 * pdf.js is mocked at the module seam - jsdom has no canvas to draw into -
 * so what is pinned is the component's half of the contract: the bytes it
 * hands over, one canvas per page, and a failure drawn in words rather
 * than an empty host.
 */
let app: Record<string, unknown> | null = null;

beforeEach(() => {
  getDocument.mockReset();
});

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  document.body.innerHTML = '';
});

function pages(count: number) {
  return {
    promise: Promise.resolve({
      numPages: count,
      getPage: (_number: number) => ({
        getViewport: ({ scale }: { scale: number }) => ({
          width: 600 * scale,
          height: 800 * scale,
        }),
        render: () => ({ promise: Promise.resolve() }),
      }),
    }),
  };
}

function draw(bytes: Uint8Array): void {
  app = mount(PdfJs, { target: document.body, props: { bytes } });
  flushSync();
}

describe('the pdf.js viewer', () => {
  it('draws one canvas per page of the bytes it was handed', async () => {
    getDocument.mockReturnValue(pages(2));
    const bytes = new TextEncoder().encode('%PDF-1.4 fake');

    draw(bytes);

    await vi.waitFor(() => {
      expect(document.querySelectorAll('canvas')).toHaveLength(2);
    });
    const handed = getDocument.mock.calls[0]?.[0] as { data: Uint8Array };
    expect(Array.from(handed.data), "the read's own bytes, not a second fetch").toEqual(
      Array.from(bytes),
    );
  });

  it('draws a failure in words rather than an empty page', async () => {
    getDocument.mockReturnValue({ promise: Promise.reject(new Error('broken file')) });

    draw(new Uint8Array([1, 2, 3]));

    await vi.waitFor(() => {
      expect(document.querySelector('.doc-said')?.textContent).toContain('broken file');
    });
  });
});
