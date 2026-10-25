/**
 * The seat's batch work: the CLI's live tasks, joined to the OS walk.
 *
 * **The row is the task's, and only the task's.** A running call is the unit
 * a reader cares about here - something that was started and is still running
 * - and the CLI announces exactly those: its background registry carries the
 * backgrounded ones, and a long foreground call reaches `task_started` and is
 * ended by its own notification. The walk supplies each row's figure: a
 * task row adopts a scanned process by the same matching rule the view this
 * client replaced used, PORTED into `cmdline.ts` rather than approximated -
 * whitespace-normalized, wrapper-unwrapped, quotes un-escaped, an empty
 * command matching nothing - and the figure counts that process AND its
 * descendant family: a cargo over four rustc workers reads as what they all
 * hold together. A row the walk missed still draws, without a figure. No
 * other process ever appears: MCP servers and detached work are not batch
 * work, and the full tree is not this row's job.
 *
 * Held as the record is: one seat is on screen, so one pair, replaced whole
 * when either side moves. The live set carries only what is running - a
 * finished task leaves it - so a row's absence is the settlement. **A task the
 * `TaskStop` tool stopped leaves no change behind**, so the conversation's own
 * stop is what settles that row (`#verdictOf`).
 */

import { untrack } from 'svelte';
import { SvelteMap, SvelteSet } from 'svelte/reactivity';

import type { BackgroundTask, ProcessSnapshot } from '../session/wire';
import { processMatchesCommand } from './cmdline';
import type { Outcome, Outcomes } from './outcomes';

/** The verdict a task the conversation says stopped carries. */
const STOPPED: Outcome = { failed: false, ended_ms: null, stopped: true };

/** One row of the batch list: a live task and the figure its process gave. */
export interface ProcessRow {
  /** Stable across beats: the task's own id. */
  key: string;
  /** The line the row leads with: the CLI's own description. */
  headline: string;
  /** What the row runs, or the kind when the call carried no command. */
  command: string;
  /** The task kind, shown where no figure was measured. */
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
  #tasks = $state<BackgroundTask[]>([]);
  /** Calls that left the live set while the turn ran: settled, until it closes. */
  #settled = $state<BackgroundTask[]>([]);
  /** The conversation's task facts, handed in with each sync: verdicts by
   * call, and the call by task, which fills an id the task row lacks. */
  #outcomes: Outcomes = {
    calls: new SvelteMap(),
    owners: new SvelteMap(),
    stoppedTasks: new SvelteSet(),
  };

  /** Take the seat's pair whole: what the record holds is what a row reads. */
  sync(
    walk: ProcessSnapshot | null | undefined,
    tasks: readonly BackgroundTask[] | null | undefined,
    turnLive: boolean,
    outcomes: Outcomes = {
      calls: new SvelteMap(),
      owners: new SvelteMap(),
      stoppedTasks: new SvelteSet(),
    },
  ): void {
    this.#outcomes = outcomes;
    const next = [...(tasks ?? [])];
    // Read the store's own prior state UNTRACKED: a caller may be an effect
    // (the session page's sync effect), and a tracked read here would make
    // that effect depend on what this same call goes on to write - an
    // update loop, not a join.
    const [previous, priorSettled] = untrack(() => [this.#tasks, this.#settled] as const);
    // The turn's own edge is what bounds the settled side: a call that
    // settles mid-turn is kept until the turn closes, then cleaned - the
    // agents rule, applied here because the strip no longer dies with the
    // turn to clean it implicitly. A closing turn cleans WITHOUT detecting:
    // the absence pass below would re-settle everything that left with it.
    let settled: BackgroundTask[] = [];
    if (turnLive) {
      settled = [...priorSettled];
      const live = new SvelteSet(next.map((task) => task.task_id));
      // The live set arrives whole each change, so a task that was held and
      // is gone has settled. It keeps its words, held from the row it was.
      for (const held of previous) {
        if (held.task_type !== 'local_bash' || live.has(held.task_id)) continue;
        if (!settled.some((task) => task.task_id === held.task_id)) settled.push(held);
      }
    }
    this.#settled = settled;
    this.#walk = walk ?? null;
    this.#tasks = next;
  }

  /**
   * The live task rows this row owns: batch calls only. An agent kind is the
   * agents row's, and a workflow is not batch work - the filter the view this
   * client replaced used, so the two rows can never list the same task.
   */
  #batches(): BackgroundTask[] {
    return this.#tasks.filter((task) => task.task_type === 'local_bash');
  }

  /**
   * The verdict the conversation has on a live task, when it has one.
   *
   * **The live set is not always the whole truth.** A task stopped by the
   * `TaskStop` tool sends no `background_tasks_changed` (4b35ed0e: the strip
   * read "1 running" over a bash with no process behind it after the tool
   * answered "Successfully stopped task"), so the conversation's own answer
   * is what ends the row.
   *
   * **By the stop's own task id first.** The record holds the newest turns
   * only, so a task started earlier in the session - a dev server, a long
   * gate - has its launch outside the page and no owner to resolve; the stop
   * still names the task, and that is what stops the count.
   */
  #verdictOf(task: BackgroundTask): Outcome | null {
    if (this.#outcomes.stoppedTasks.has(task.task_id)) return STOPPED;
    const call = this.#callOf(task);
    return call === null ? null : (this.#outcomes.calls.get(call) ?? null);
  }

  /** The batches nothing has ended: what the row counts and meters. */
  #live(): BackgroundTask[] {
    return this.#batches().filter((task) => this.#verdictOf(task)?.stopped !== true);
  }

  /** How many batch calls are running: the set holds the living, and a
   * stopped task is not one of them however long the set lags. */
  running(): number {
    return this.#live().length;
  }

  /** How many settled this turn and are still shown, a stopped-but-registered
   * task among them. */
  settledCount(): number {
    return this.#settled.length + this.#batches().length - this.#live().length;
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
   * depth-0 rule the view this client replaced kept, applied per row.
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
   * One task row adopts at most one process - the first unclaimed one
   * its command names - so two calls sharing a command do not both wear the
   * same process's figure.
   */
  rows(): ProcessRow[] {
    const walk = this.#walk?.processes ?? [];
    const claimed = new SvelteSet<number>();
    const rows: ProcessRow[] = this.#batches().map((task) => {
      // A stopped task adopts no process: the walk would be matching whatever
      // took the command's name next, and the row is not running anything.
      const stopped = this.#verdictOf(task)?.stopped === true;
      const match = stopped
        ? undefined
        : walk.find(
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
        settled: stopped,
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
   * The call a live task belongs to.
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
