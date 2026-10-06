import { describe, expect, it } from 'vitest';

import {
  commandHaystack,
  commandNeedle,
  extractInnerCommand,
  processMatchesCommand,
} from './cmdline';

/** The real wrapper shape, captured via `ps -axww`. */
const wrapper = (inner: string) =>
  `/bin/zsh -c source /x/.claude/shell-snapshots/snap.sh 2>/dev/null || true && eval '${inner}' < /dev/null && pwd -P >| /tmp/claude-x-cwd`;

describe("the terminal's command-matching rule, ported", () => {
  it('unwraps the shell wrapper to the user command', () => {
    expect(extractInnerCommand(wrapper('cargo nextest run')), 'the inner command').toBe(
      'cargo nextest run',
    );
    expect(extractInnerCommand('cargo nextest run'), 'not a wrapper').toBe(null);
  });

  it('un-escapes the wrapper single-quote escape', () => {
    // The wrapper re-escapes each `'` as `'"'"'`; without reversing it, a
    // quoted command never matches its own call's text.
    expect(
      extractInnerCommand(wrapper(`echo '"'"'sq-marker'"'"'; sleep 40`)),
      'the recovered command reads verbatim',
    ).toBe("echo 'sq-marker'; sleep 40");
  });

  it('terminates at the OUTERMOST redirect, not the first it meets', () => {
    const inner = `echo '"'"'hi'"'"' < /dev/null`;
    expect(extractInnerCommand(wrapper(inner)), 'the last redirect is the terminator').toBe(
      `echo 'hi' < /dev/null`,
    );
  });

  it('normalizes whitespace on both sides of the match', () => {
    expect(commandNeedle('cargo  nextest\n run '), 'the needle trims and collapses').toBe(
      'cargo nextest run',
    );
    expect(commandHaystack('cargo  nextest\n run'), 'so does the haystack').toBe(
      'cargo nextest run',
    );
  });

  it('matches a quoted command against its own wrapper', () => {
    // The finding's worked case: the wire command carries plain quotes, the
    // scanned cmdline carries the escape, and the row must still adopt it.
    expect(
      processMatchesCommand(
        wrapper(`git commit -m '"'"'fix auth timeout'"'"'`),
        "git commit -m 'fix auth timeout'",
      ),
    ).toBe(true);
  });

  it('refuses an empty needle rather than matching the first process', () => {
    expect(processMatchesCommand(wrapper('anything at all'), ''), 'the empty needle').toBe(false);
    expect(processMatchesCommand(wrapper('anything at all'), '   '), 'and the blank one').toBe(
      false,
    );
  });

  it('does not match a command the process is not running', () => {
    expect(
      processMatchesCommand(wrapper('cargo build'), 'cargo nextest run'),
      'a different command does not match',
    ).toBe(false);
  });
});
