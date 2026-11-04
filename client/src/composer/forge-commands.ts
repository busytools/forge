/**
 * forge's own slash commands: the catalogue the `/` list offers beside the ones
 * the CLI advertises.
 *
 * **Static, and it does not cross the wire.** The table is the same for every
 * reader, so it ships in the bundle and the dropdown opens without a round
 * trip; what the server sends for a seat is the commands the CLI advertised
 * there, which is a fact about that session.
 *
 * The table is `crates/forge-server/src/commands.rs`'s, alphabetical by name,
 * which is the order the list draws them in; a name here shadows the CLI's
 * row for the same name. `/usage` has no forge handler behind it since the
 * page that answered it here was removed, so its description states the
 * CLI's own answer, which is what typing the name draws now.
 */

/** One command in the catalogue: the name as it is typed, and what it does. */
interface ForgeCommand {
  name: string;
  description: string;
}

export const FORGE_COMMANDS: ForgeCommand[] = [
  { name: '/compact', description: 'Compact session context' },
  { name: '/effort', description: 'Show / set thinking effort' },
  { name: '/handoff', description: 'Hand off to a fresh session' },
  { name: '/mode', description: 'Show / set session mode' },
  { name: '/model', description: 'Show / set session model' },
  { name: '/new', description: 'Start a fresh session' },
  { name: '/resume', description: 'Resume a session by ID' },
  { name: '/usage', description: 'Show session cost and plan usage' },
];

/** Whether `name` is in the catalogue, so one command is not drawn twice. */
export function isForgeCommand(name: string): boolean {
  return FORGE_COMMANDS.some((command) => command.name === name);
}

/**
 * The names the core answers itself, and so draws no turn row for.
 *
 * `crates/forge-workspace/src/prompt.rs`'s usage table, not this catalogue:
 * `/compact` and `/usage` are offered here but forwarded to the CLI, which
 * does start a turn for them.
 */
const ANSWERED = new Set(['/new', '/resume', '/handoff', '/mode', '/model', '/effort']);

/** The names the CLI resolves itself though it does not advertise them. */
const FORWARDED = new Set(['/help', '/plugins', '/quit', '/mcp']);

/**
 * Whether the core answers this prompt itself, so no turn carries its words.
 *
 * Mirrors the dispatcher's rule: a forge name it handles, or a slash name this
 * session cannot run. **An empty catalogue is forwarded**, because the core
 * cannot tell an unknown name from one the CLI has not advertised yet.
 */
export function coreAnswers(name: string, advertised: readonly string[]): boolean {
  if (ANSWERED.has(name)) return true;
  if (!name.startsWith('/')) return false;
  if (advertised.length === 0) return false;
  return !advertised.includes(name) && !FORWARDED.has(name);
}
