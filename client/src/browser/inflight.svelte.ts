/**
 * How many browser calls are in flight, for the strip's live mark.
 *
 * The state exists for as long as a call does and is about the CLIENT, not a
 * session: every ask the client answers passes through `hostTheBrowser`, so
 * the counter lives beside that wrapper and the strip reads it for the same
 * ring the conversation draws for work in flight.
 */
export const browserInflight = $state({ calls: 0 });
