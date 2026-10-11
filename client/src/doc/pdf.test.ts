import { describe, expect, it } from 'vitest';

import { needsPdfJs } from './pdf';

const ANDROID =
  'Mozilla/5.0 (Linux; Android 15; Pixel 9) AppleWebKit/537.36 (KHTML, like Gecko) ' +
  'Version/4.0 Chrome/126.0.0.0 Mobile Safari/537.36';
const MAC =
  'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) ' +
  'Version/18.0 Safari/605.1.15';

describe('the pdf.js gate', () => {
  /**
   * **The Android WebView is the one surface with no PDF viewer**, and it
   * is the SHELL half of that check that matters: Chrome on Android draws
   * PDFs natively, so an Android browser is not a surface that needs
   * pdf.js - only the app is.
   */
  it('is needed on the phone shell and nowhere else', () => {
    expect(needsPdfJs(ANDROID, true), 'the app on Android has no viewer of its own').toBe(true);
    expect(needsPdfJs(ANDROID, false), 'an Android browser renders PDFs natively').toBe(false);
    expect(needsPdfJs(MAC, true), 'the desktop webview renders PDFs natively').toBe(false);
    expect(needsPdfJs(MAC, false)).toBe(false);
  });
});
