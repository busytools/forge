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
 * which is the order the list draws them in. A name here shadows the CLI's row
 * for the same name: forge handles those itself rather than forwarding them.
 */

import type { Route } from '../routes';

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
  { name: '/usage', description: 'Token/cost usage by project or model' },
];

/** Whether forge handles `name` itself, so one command is not drawn twice. */
export function isForgeCommand(name: string): boolean {
  return FORGE_COMMANDS.some((command) => command.name === name);
}

/**
 * What a typed forge command does in THIS view, before anything is sent:
 * the route it opens here, or the line it answers a wrong invocation with.
 *
 * The view this client replaced had the same submit path: a name that was a
 * page there is a route here, and a name invoked with arguments it does not
 * take is answered with its usage line rather than reaching the model as
 * prose.
 * A name this table does not answer - `/new`, `/model`, and the rest of the
 * core's - returns `null` and crosses the wire as it always did.
 */
export type CommandMove = { kind: 'route'; route: Route } | { kind: 'line'; text: string };

export function routeForCommand(text: string): CommandMove | null {
  const [name = '', ...args] = text.trim().split(/\s+/);
  if (name !== '/usage') return null;
  if (args.length > 0) return { kind: 'line', text: 'Usage: /usage' };
  return { kind: 'route', route: { name: 'usage' } };
}
