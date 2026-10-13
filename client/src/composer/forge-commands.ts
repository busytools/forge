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

/** One command forge handles itself: the name as it is typed, and what it does. */
export interface ForgeCommand {
  name: string;
  description: string;
}

export const FORGE_COMMANDS: ForgeCommand[] = [
  { name: '/compact', description: 'Compact session context' },
  { name: '/effort', description: 'Set thinking effort' },
  { name: '/mode', description: 'Set session mode' },
  { name: '/model', description: 'Show / set session model' },
  { name: '/new', description: 'Start a fresh session' },
  { name: '/resume', description: 'Resume a session by ID' },
  { name: '/usage', description: 'Show session cost and plan usage' },
];

/** Whether forge handles `name` itself, so one command is not drawn twice. */
export function isForgeCommand(name: string): boolean {
  return FORGE_COMMANDS.some((command) => command.name === name);
}
