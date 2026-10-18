/**
 * The heartbeat's tick interval, in one place (#2374).
 *
 * The Web Worker that ticks (`heartbeat.worker.ts`) and the probe that reads
 * the ticks (`perf.ts`) must agree on it: the record's beat count is read
 * against this interval, so two constants drifting apart would make every
 * reading wrong without anything failing.
 */

export const HEARTBEAT_TICK_MS = 1_000;
