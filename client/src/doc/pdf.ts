/**
 * The PDF surface decision: whether this client has to draw a PDF itself.
 *
 * The desktop webview (WKWebView) and every browser render an embedded PDF
 * natively - both measured on 2026-10-11, a real PDF against a blank
 * control - and Firefox ships its own pdf.js. The Android WebView is the
 * one client surface with no PDF viewer at all, so only there does pdf.js
 * draw the pages.
 */

/**
 * Whether a PDF needs pdf.js here. Both halves are injectable so the gate
 * is testable without pretending to be a phone.
 */
export function needsPdfJs(
  userAgent: string = navigator.userAgent,
  inShell: boolean = typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window,
): boolean {
  // The shell check matters: Chrome on Android renders PDFs natively, so an
  // Android BROWSER is not a surface that needs pdf.js - only the app.
  return inShell && /Android/i.test(userAgent);
}
