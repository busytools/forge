/**
 * The clipboard a landing writes through, on every client the page runs on.
 *
 * **A landing is not a user activation.** The desktop's webview refuses an
 * async `navigator.clipboard.writeText` unless a gesture is under it, and a
 * take lands seconds after the stop press, so the shell writes through
 * Tauri's clipboard plugin instead. A browser page has no shell: an origin the
 * browser trusts (`https` or `localhost`) gets `navigator.clipboard`, and one
 * it does not gets a selection copied with `document.execCommand`.
 *
 * **Nothing is drawn on any path**, and no failure is the reader's to see: a
 * refused write costs only the copy, and it is logged at debug.
 */

import { canHost } from './browser/host';

/** Write the landed words to the clipboard, silently. */
export async function copyToClipboard(text: string): Promise<void> {
  if (canHost()) {
    await copyThroughShell(text);
    return;
  }
  const clipboard = navigator.clipboard;
  if (clipboard !== undefined) {
    try {
      await clipboard.writeText(text);
    } catch (why) {
      console.debug('the clipboard refused the landed words', why);
    }
    return;
  }
  copyThroughSelection(text);
}

/**
 * The shell's clipboard plugin, imported lazily so a page outside the shell
 * never loads it.
 */
async function copyThroughShell(text: string): Promise<void> {
  try {
    const { writeText } = await import('@tauri-apps/plugin-clipboard-manager');
    await writeText(text);
  } catch (why) {
    console.debug('the shell clipboard refused the landed words', why);
  }
}

/**
 * A selection copied from a box the page never shows: the last best-effort
 * path for a non-secure origin, where no clipboard API exists.
 */
function copyThroughSelection(text: string): void {
  if (typeof document.execCommand !== 'function') return;
  const box = document.createElement('textarea');
  box.value = text;
  // **Off screen rather than hidden.** A `display: none` box cannot be
  // selected, and an unselected copy is a no-op; `readonly` keeps a phone's
  // keyboard down for the frame the box exists.
  box.setAttribute('readonly', '');
  box.style.position = 'fixed';
  box.style.top = '0';
  box.style.left = '-9999px';
  document.body.appendChild(box);
  box.select();
  try {
    document.execCommand('copy');
  } catch (why) {
    console.debug('the copied selection was refused', why);
  } finally {
    box.remove();
  }
}
