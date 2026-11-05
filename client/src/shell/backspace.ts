/**
 * Cancel a bare Backspace's default action, which macOS WebKit turns into its
 * own history navigation.
 *
 * The page's history entries are the router's; WebKit runs a keydown's default
 * action only when the event was not cancelled, so cancelling this one keeps
 * the webview's history where the router put it. The defect it closes: a take
 * replaces the field it was typed in with its card, and the next Backspace
 * moved the reader a whole session back.
 */
export function guardBackspaceNavigation(): () => void {
  const guard = (event: KeyboardEvent): void => {
    if (event.key !== 'Backspace') return;
    if (ownsText(event.target)) return;
    event.preventDefault();
  };
  window.addEventListener('keydown', guard, true);
  return () => window.removeEventListener('keydown', guard, true);
}

/** The input types a Backspace deletes in. */
const TEXT_INPUTS = new Set([
  'date',
  'datetime-local',
  'email',
  'month',
  'number',
  'password',
  'search',
  'tel',
  'text',
  'time',
  'url',
  'week',
]);

/** Whether the key's target is a text editor, where Backspace is its own. */
function ownsText(target: EventTarget | null): boolean {
  if (target instanceof HTMLTextAreaElement) return true;
  if (target instanceof HTMLInputElement) return TEXT_INPUTS.has(target.type);
  return target instanceof HTMLElement && target.isContentEditable;
}
