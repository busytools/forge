/**
 * Stop a bare Backspace from moving the client the way WebKit would.
 *
 * **macOS WebKit navigates its own history on a Backspace that no editor owns**
 * (`BackspaceKeyNavigationEnabled` defaults on there), and the client is an
 * SPA whose history entries are the router's. A take replaces the field it was
 * typed in with its card, so the next Backspace has no editor to stay in and
 * walked the reader a whole session back (live, 2026-11-05).
 *
 * The default action is the webview's, so it is cancelled at the page, where
 * the keydown is dispatched: `preventDefault` on a key the DOM does not
 * default-handle stops WebKit's own handler running.
 */
export function guardBackspaceNavigation(): () => void {
  const guard = (event: KeyboardEvent): void => {
    if (event.key !== 'Backspace') return;
    if (holdsText(event.target)) return;
    event.preventDefault();
  };
  window.addEventListener('keydown', guard, true);
  return () => window.removeEventListener('keydown', guard, true);
}

/** Whether the key's target is something a Backspace is deleting in. */
function holdsText(target: EventTarget | null): boolean {
  if (target instanceof HTMLInputElement || target instanceof HTMLTextAreaElement) return true;
  return target instanceof HTMLElement && target.isContentEditable;
}
