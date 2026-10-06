/**
 * The seat's batch work: the CLI's registry, joined to the OS walk.
 *
 * **The row is the registry's, and only the registry's.** A backgrounded
 * call is the unit a reader cares about here - something that was started
 * and is still running - and the registry names exactly those. The walk
 * supplies each row's figure: a registry row adopts a scanned process by
 * the terminal's own matching rule, PORTED into `cmdline.ts` rather than
 * approximated - whitespace-normalized, wrapper-unwrapped, quotes
 * un-escaped, an empty command matching nothing - and the figure counts
 * that process AND its descendant family: a cargo over four rustc workers
 * reads as what they all hold together. A row the walk missed still draws,
 * without a figure. No other process ever appears: MCP servers and detached
 * work are not batch work, and the full tree is not this row's job.
 *
 * Held as the record is: one seat is on screen, so one pair, replaced whole
 * when either side moves. The registry carries only what is running - a
 * finished task leaves it - so a row's absence is the settlement.
 */

import { untrack } from 'svelte';
import { SvelteMap, SvelteSet } from 'svelte/reactivity';

import type { BackgroundTask, ProcessSnapshot } from '../session/wire';
import { processMatchesCommand } from './cmdline';
import type { Outcomes } from './outcomes';

/** One row of the batch list: a registry row and the figure its process gave. */
export interface ProcessRow {
  /** Stable across beats: the registry's task id. */
  key: string;
  /** The line the row leads with: the CLI's own description. */
  headline: string;
  /** What the row runs, or the kind when the call carried no command. */
  command: string;
  /** The registry kind, shown where no figure was measured. */
  kind: string;
  /** The tool call that began the task, which the row click reveals. */
  tool_use_id: string | null;
  /** Whether the call has finished: kept this turn, then cleaned. */
  settled: boolean;
  /** The CLI's verdict on a settled call: only `failed` marks it. */
  failed: boolean;
  /** When a settled call ended (unix ms), from the task frames. */
  ended_ms: number | null;
  /** Resident memory of the matched process's whole family, when the walk
   * found it: the number that explains the call, not one worker's own RSS. */
  memory_bytes: number | null;
  pid: number | null;
}

export class Processes {
  #walk = $state<ProcessSnapshot | null>(null);
  #registry = $state<BackgroundTask[]>([]);
  /** Calls that left the registry while the turn ran: settled, until it closes. */
  #settled = $state<BackgroundTask[]>([]);
  /** The conversation's task facts, handed in with each sync: verdicts by
   * call, and the call by task, which fills an id the registry row lacks. */
  #outcomes: Outcomes = { calls: new SvelteMap(), owners: new SvelteMap() };

  /** Take the seat's pair whole: what the record holds is what a row reads. */
  sync(
    walk: ProcessSnapshot | null | undefined,
    registry: readonly BackgroundTask[] | null | undefined,
    turnLive: boolean,
    outcomes: Outcomes = { calls: new SvelteMap(), owners: new SvelteMap() },
  ): void {
    this.#outcomes = outcomes;
    const next = [...(registry ?? [])];
    // Read the store's own prior state UNTRACKED: a caller may be an effect
    // (the session page's sync effect), and a tracked read here would make
    // that effect depend on what this same call goes on to write - an
    // update loop, not a join.
    const [previous, priorSettled] = untrack(() => [this.#registry, this.#settled] as const);
    // The turn's own edge is what bounds the settled side: a call that
    // settles mid-turn is kept until the turn closes, then cleaned - the
    // agents rule, applied here because the strip no longer dies with the
    // turn to clean it implicitly. A closing turn cleans WITHOUT detecting:
    // the absence pass below would re-settle everything that left with it.
    let settled: BackgroundTask[] = [];
    if (turnLive) {
      settled = [...priorSettled];
      const live = new SvelteSet(next.map((task) => task.task_id));
      // The registry arrives whole each change, so a task that was held and
      // is gone has settled. It keeps its words, held from the row it was.
      for (const held of previous) {
        if (held.task_type !== 'local_bash' || live.has(held.task_id)) continue;
        if (!settled.some((task) => task.task_id === held.task_id)) settled.push(held);
      }
    }
    this.#settled = settled;
    this.#walk = walk ?? null;
    this.#registry = next;
  }

  /**
   * The registry rows this row owns: batch calls only. An agent kind is the
   * agents row's, and a workflow is not batch work - the terminal's own
   * filter, so the two rows can never list the same task.
   */
  #batches(): BackgroundTask[] {
    return this.#registry.filter((task) => task.task_type === 'local_bash');
  }

  /** How many batch calls are running: the registry holds only the living. */
  running(): number {
    return this.#batches().length;
  }

  /** How many settled this turn and are still shown. */
  settledCount(): number {
    return this.#settled.length;
  }

  /** Whether anything is out at all, a settled row included. */
  anything(): boolean {
    return this.#batches().length > 0 || this.#settled.length > 0;
  }

  /** What every running call's family holds together: the one memory number
   * a reader can act on - a shape without the machine's own total invites a
   * question it cannot answer. */
  totalBytes(): number {
    return this.rows()
      .filter((row) => !row.settled)
      .reduce((sum, row) => sum + (row.memory_bytes ?? 0), 0);
  }

  /**
   * The resident memory of `pid` and every descendant of it in the walk.
   *
   * The row's figure is the whole family, not the matched process's own
   * RSS: a `cargo` at 400 MB over rustc workers at 1.4 GB reads as 400 MB on
   * its own line, which is the one number that explains nothing. The
   * terminal's depth-0 rule, applied per row.
   */
  #subtreeBytes(pid: number): number {
    const walk = this.#walk?.processes ?? [];
    let total = 0;
    const stack = [pid];
    const seen = new SvelteSet<number>();
    while (stack.length > 0) {
      const at = stack.pop();
      if (at === undefined || seen.has(at)) continue;
      seen.add(at);
      for (const entry of walk) {
        if (entry.pid === at) total += entry.memory_bytes;
        else if (entry.parent_pid === at) stack.push(entry.pid);
      }
    }
    return total;
  }

  /**
   * Every batch row, biggest first, a row nothing measured last.
   *
   * One registry row adopts at most one process - the first unclaimed one
   * its command names - so two calls sharing a command do not both wear the
   * same process's figure.
   */
  rows(): ProcessRow[] {
    const walk = this.#walk?.processes ?? [];
    const claimed = new SvelteSet<number>();
    const rows: ProcessRow[] = this.#batches().map((task) => {
      const match = walk.find(
        (entry) =>
          task.command !== null &&
          !claimed.has(entry.pid) &&
          processMatchesCommand(entry.command, task.command),
      );
      if (match !== undefined) claimed.add(match.pid);
      return {
        key: `task:${task.task_id}`,
        headline: task.description,
        command: task.command ?? task.task_type,
        kind: task.task_type,
        tool_use_id: this.#callOf(task),
        settled: false,
        failed: false,
        ended_ms: null,
        memory_bytes: match === undefined ? null : this.#subtreeBytes(match.pid),
        pid: match?.pid ?? null,
      };
    });
    // Biggest first, and a row with no figure (a call the walk missed) sorts
    // last rather than as a zero: nothing was measured, which is not the
    // same as measuring nothing.
    rows.sort((a, b) => (b.memory_bytes ?? -1) - (a.memory_bytes ?? -1));
    // The settled rows follow the running ones, in the order they finished,
    // each wearing the verdict the frames carried for its call.
    for (const task of this.#settled) {
      const call = this.#callOf(task);
      const outcome = call === null ? undefined : this.#outcomes.calls.get(call);
      rows.push({
        key: `task:${task.task_id}`,
        headline: task.description,
        command: task.command ?? task.task_type,
        kind: task.task_type,
        tool_use_id: call,
        settled: true,
        failed: outcome?.failed ?? false,
        ended_ms: outcome?.ended_ms ?? null,
        memory_bytes: null,
        pid: null,
      });
    }
    return rows;
  }

  /**
   * The call a registry row belongs to.
   *
   * The row's own id is often null by wire order - the CLI announces the
   * task BEFORE it names its call, and the snapshot the client holds is the
   * earlier one - so the conversation's `task_started` link, which the
   * client reads anyway, is where the row finds its call.
   */
  #callOf(task: BackgroundTask): string | null {
    return task.tool_use_id ?? this.#outcomes.owners.get(task.task_id) ?? null;
  }
}

/** The one seat's pair, as the record last stated it. */
export const processes = new Processes();
