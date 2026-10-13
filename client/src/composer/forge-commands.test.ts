import { describe, expect, it } from 'vitest';

import { routeForCommand } from './forge-commands';

describe('what a typed forge command does in this view', () => {
  /**
   * `/usage` opens the usage page here rather than crossing the wire: the
   * view this client replaced opened its overlay on the bare name, and the
   * words never reach the model.
   */
  it('routes the bare name to the page', () => {
    expect(routeForCommand('/usage')).toEqual({ kind: 'route', route: { name: 'usage' } });
    expect(routeForCommand('  /usage  '), 'the ends are trimmed like a submit').toEqual({
      kind: 'route',
      route: { name: 'usage' },
    });
  });

  /** The name takes no arguments, and the view this client replaced answered
   * that with its usage line rather than opening - so a wrong invocation is
   * answered here too, and still never reaches the model. */
  it('answers a name given arguments with its usage line', () => {
    expect(routeForCommand('/usage today')).toEqual({ kind: 'line', text: 'Usage: /usage' });
  });

  /** The name is a whole word, and every other command is the core's: the
   * interception must not swallow what the server answers. */
  it('leaves every other text to the core', () => {
    expect(routeForCommand('/usagex')).toBeNull();
    expect(routeForCommand('usage')).toBeNull();
    expect(routeForCommand('/extensions')).toBeNull();
    expect(routeForCommand('/new')).toBeNull();
    expect(routeForCommand('hello there')).toBeNull();
    expect(routeForCommand('')).toBeNull();
  });
});
