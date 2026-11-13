/**
 * One row a page-drawn picker offers.
 *
 * **Its own module rather than a `<script module>` export**: the client's
 * type check reads a `.svelte` file as a component, not as a module with
 * exports, so a caller that names the row type imports it from here.
 */
export interface PickOption {
  value: string;
  label: string;
  /** What the row is, said beside its name - a worker's kind, a scope. */
  note?: string;
}
