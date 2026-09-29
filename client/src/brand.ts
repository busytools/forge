/**
 * The marks forge ships, by the name `[web] mark` takes - the client half of
 * `crates/forge-web/src/brand.rs`.
 *
 * This is the mark the home draws beside the wordmark, not the lifecycle mark
 * a row carries: the two are unrelated and share only the word.
 *
 * Every mark is one `<svg viewBox="0 0 24 24">` drawn in `currentColor`, so
 * switching between the accent and a single colour is a `color` change on the
 * caller and nothing else.
 */

const PANES =
  '<g fill="none" stroke="currentColor" stroke-width="2.4"><rect x="3.2" y="3.2" width="17.6" height="17.6" rx="4"/><path d="M13.6 3.2V20.8"/></g><path fill="currentColor" d="M4.4 6.4a2 2 0 0 1 2-2h4v15.2h-4a2 2 0 0 1-2-2Z"/>';

/** The shapes inside the `<svg>`, by shipped name. */
const PATHS: Record<string, string> = {
  panes: PANES,
  klin: '<path fill="currentColor" fill-rule="evenodd" d="M5 3h14a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2Zm2.5 19v-8.5a4.5 4.5 0 0 1 9 0V22h-9Z"/>',
  lanes:
    '<g fill="currentColor"><rect x="2.5" y="8.5" width="4" height="12.5" rx="2"/><rect x="10" y="3" width="4" height="18" rx="2"/><rect x="17.5" y="12" width="4" height="9" rx="2"/></g>',
  split:
    '<g fill="currentColor"><rect x="9" y="2.5" width="6" height="4.5" rx="2"/><rect x="10.6" y="6.4" width="2.8" height="3"/><rect x="3" y="8.8" width="18" height="3" rx="1.5"/><rect x="4.25" y="11.8" width="3" height="9.7" rx="1.5"/><rect x="10.5" y="11.8" width="3" height="9.7" rx="1.5"/><rect x="16.75" y="11.8" width="3" height="9.7" rx="1.5"/></g>',
  spine:
    '<g fill="currentColor"><rect x="4" y="2.5" width="3.5" height="19" rx="1.75"/><rect x="7.5" y="6.6" width="7" height="3" rx="1"/><rect x="14.5" y="5.1" width="6" height="6" rx="1.8"/><rect x="7.5" y="14.4" width="7" height="3" rx="1"/><rect x="14.5" y="12.9" width="6" height="6" rx="1.8"/></g>',
  grid: '<g fill="none" stroke="currentColor" stroke-width="2.2"><rect x="4.1" y="4.1" width="5.3" height="5.3" rx="1.5"/><rect x="14.6" y="4.1" width="5.3" height="5.3" rx="1.5"/><rect x="14.6" y="14.6" width="5.3" height="5.3" rx="1.5"/></g><rect x="4.1" y="14.6" width="5.3" height="5.3" rx="1.5" fill="currentColor"/>',
  clamp:
    '<g fill="currentColor"><rect x="2.5" y="3" width="19" height="4" rx="2"/><rect x="6.5" y="9.5" width="11" height="5" rx="1.6"/><rect x="2.5" y="17" width="19" height="4" rx="2"/></g>',
  strike:
    '<g fill="currentColor"><rect x="9.9" y="1" width="4.2" height="22" rx="2.1" transform="rotate(45 12 12)"/><rect x="9.9" y="1" width="4.2" height="22" rx="2.1" transform="rotate(-45 12 12)"/></g>',
  nest: '<g fill="none" stroke="currentColor" stroke-width="2.2"><rect x="3.9" y="3.9" width="16.2" height="16.2" rx="4"/><rect x="9" y="9" width="6" height="6" rx="1.8"/></g>',
  chamfer:
    '<path fill="currentColor" d="M7 3h9l5 5v9a4 4 0 0 1-4 4H7a4 4 0 0 1-4-4V7a4 4 0 0 1 4-4Z"/>',
  tally:
    '<g fill="none" stroke="currentColor" stroke-width="2.6" stroke-linecap="round"><path d="M3.5 6.5V17.5M8.5 5V19M13.5 5V19M18.5 6.5V17.5M2.4 17.2 21.6 6.8"/></g>',
  stencil_f:
    '<path fill="currentColor" fill-rule="evenodd" d="M6.5 3h11a3.5 3.5 0 0 1 3.5 3.5v11a3.5 3.5 0 0 1-3.5 3.5h-11a3.5 3.5 0 0 1-3.5-3.5v-11a3.5 3.5 0 0 1 3.5-3.5Zm1 3.5h3.5v11h-3.5Zm3.5 0h7v3.5h-7Zm0 5h5v3.5h-5Z"/>',
  f_slab:
    '<g fill="currentColor"><rect x="4" y="3.5" width="16" height="4.5" rx="1.4"/><rect x="4" y="10.5" width="11" height="4.5" rx="1.4"/><rect x="4" y="3.5" width="4.5" height="17" rx="1.4"/></g>',
};

/**
 * The body for a name, and the built-in for an unset one.
 *
 * The server refuses a name outside the shipped set at boot, so the fallback
 * is the renderer's backstop rather than a path a config reaches.
 */
export function brandPath(name: string | null): string {
  if (name === null) return PANES;
  return PATHS[name] ?? PANES;
}
