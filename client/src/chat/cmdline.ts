/**
 * The terminal's own command-matching rule, ported from
 * `forge-agent/src/env/processes.rs`.
 *
 * A registry row adopts a scanned process by its command, and the raw
 * substring check is not that rule: the shell wrapper re-escapes every quote
 * in a backgrounded command (`git commit -m 'fix auth timeout'` arrives as
 * `'"'"'fix auth timeout'"'"'`), a multi-line or multi-space command never
 * matches byte-for-byte, and an empty needle would adopt the first process it
 * meets. The ported rule normalizes both sides' whitespace, unwraps the
 * wrapper to the user command, un-escapes single quotes, and refuses an empty
 * needle outright - the same answers the terminal gives, pinned on both sides
 * by the cases below and their Rust originals.
 */

/** Every run of whitespace collapsed to one space, ends trimmed. Private,
 *  like the Rust original: the rule's callers are its two users below. */
function normalizeCommandWs(value: string): string {
  return value.split(/\s+/).filter(Boolean).join(' ');
}

/**
 * The user command inside a recognized shell wrapper, or null.
 *
 * The inner command is single-quoted between the FIRST `eval '` and the LAST
 * `' < /dev/null` - the outermost occurrence, so a command that itself
 * contains that redirect does not cut the unwrap short - and the wrapper's
 * POSIX single-quote escape (`'"'"'`) is reversed so the recovered text
 * matches the call's own command.
 */
export function extractInnerCommand(cmdline: string): string | null {
  const afterEval = cmdline.split("eval '")[1];
  if (afterEval === undefined) return null;
  const at = afterEval.lastIndexOf("' < /dev/null");
  if (at < 0) return null;
  return afterEval.slice(0, at).trim().replaceAll(`'"'"'`, "'");
}

/** The normalized command text a registry row is matched on. */
export function commandNeedle(command: string): string {
  return normalizeCommandWs(command);
}

/** The normalized cmdline a process is matched on: its unwrapped inner
 *  command for a wrapper, else the cmdline itself. */
export function commandHaystack(cmdline: string): string {
  return normalizeCommandWs(extractInnerCommand(cmdline) ?? cmdline);
}

/** Whether a scanned process's cmdline plausibly is the work a registry
 *  row's command names. An empty needle matches nothing. */
export function processMatchesCommand(processCmd: string, command: string): boolean {
  const needle = commandNeedle(command);
  if (needle === '') return false;
  return commandHaystack(processCmd).includes(needle);
}
