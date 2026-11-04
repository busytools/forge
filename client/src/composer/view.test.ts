import { describe, expect, it } from 'vitest';

import type { DictateWire } from '../wire/home';
import { seatRead, take } from './testing';
import {
  blocked,
  dictationOffered,
  draftEndingLine,
  settingsWriteEndingLine,
  vaultAskEndingLine,
} from './view';
import { composerFrom, NO_COMPOSER } from './wire';

/** The home's dictate read, with whatever the test overrides. */
function dictate(over: Partial<DictateWire> = {}): DictateWire {
  return {
    enabled: true,
    snapshot: { models: [], failure: null },
    models_dir: null,
    ...over,
  };
}

const model = {
  name: 'base',
  state: 'ready' as const,
  size_bytes: 0,
  path: null,
  failure: null,
  role: 'transcribe' as const,
  file: '',
};

/**
 * Whether the composer offers the way into a take.
 *
 * Both halves are read because the wire carries both, and an empty model list
 * means two different things: a switched-off `[dictate]`, and a snapshot taken
 * before the models finished loading. A mic drawn for either is a control this
 * install cannot honour.
 */
describe('whether dictation is on offer', () => {
  it('needs the section switched on AND every model loaded', () => {
    expect(
      dictationOffered(dictate({ snapshot: { models: [model], failure: null } })),
      'on, with its model ready',
    ).toBe(true);
    expect(
      dictationOffered(dictate({ enabled: false, snapshot: { models: [model], failure: null } })),
      'the section is off',
    ).toBe(false);
    expect(dictationOffered(dictate()), 'on, with nothing declared yet').toBe(false);

    // Mid-preflight and after a failed load both draw a mic the core cannot
    // honour, because the engine is parked only once the whole set is loaded -
    // and the home's own card calls the install ready on the same reading.
    const loading = { ...model, state: 'loading' as const };
    expect(
      dictationOffered(dictate({ snapshot: { models: [loading], failure: null } })),
      'a model still loading is not a loaded one',
    ).toBe(false);
    expect(
      dictationOffered(dictate({ snapshot: { models: [model, loading], failure: null } })),
      'and one of a set still loading is not the whole set',
    ).toBe(false);
    expect(
      dictationOffered(
        dictate({ snapshot: { models: [], failure: { other: { message: 'refused' } } } }),
      ),
      'a failed load declares no model at all',
    ).toBe(false);
  });
});

/**
 * What one draft ending reads as, where the dock stood.
 *
 * The ending crosses as the core's own externally tagged enum, so a unit
 * variant is a bare name and the answered one is a name around its field.
 * Nothing else on the wire says what became of a draft this reader did not
 * answer.
 */
describe('the line a resolved draft leaves', () => {
  it('names the ending the update carried', () => {
    expect(draftEndingLine('abandoned').text, 'the asker').toContain('session went away');
    expect(
      draftEndingLine({ answered: { approved: true } }).text,
      'and whether the message went out',
    ).toContain('posted from another view');
    expect(
      draftEndingLine({ answered: { approved: false } }).text,
      'declined is its own line',
    ).toContain('declined in another view');
    expect(
      draftEndingLine('sent_back').text,
      "a send-back is neither of those, and the note is the agent's, not the view's",
    ).toContain('sent back with feedback in another view');
  });

  it('still says the draft is gone for an ending this client is older than', () => {
    expect(
      draftEndingLine({ something_new: null }).text,
      'dropping the news would leave the disappearance unexplained',
    ).toContain('no longer waiting');
    expect(draftEndingLine(undefined).text, 'and a payload that carries none').toContain(
      'no longer waiting',
    );
  });
});

/**
 * The held write's ending shares the draft ending's wire shape, so its line
 * must not borrow the draft's words: a person answering a config write from
 * a second client is not told a Slack draft was posted.
 */
describe('the line a resolved config write leaves', () => {
  it('names the write and the ending', () => {
    expect(settingsWriteEndingLine('abandoned').text).toContain("config write's asking session");
    expect(settingsWriteEndingLine('applied').text).toContain('applied from another view');
    expect(settingsWriteEndingLine('declined').text).toContain('declined in another view');
    const refused = settingsWriteEndingLine({ refused: { message: 'the file moved' } });
    expect(refused.text).toContain('approved but did not land');
    expect(refused.text).toContain('the file moved');
    expect(refused.tone, 'a failed write is not quiet news').toBe('warn');
    for (const ending of ['abandoned', 'applied', undefined]) {
      expect(settingsWriteEndingLine(ending).text, 'never the Slack line').not.toContain('Slack');
    }
  });

  it('still says the write is gone for an ending this client is older than', () => {
    expect(settingsWriteEndingLine({ something_new: null }).text).toContain('no longer waiting');
  });
});

/**
 * The held vault call's ending shares the same wire shape again, and its line
 * must name the call: a person who answered it from a second client is not
 * told a config write happened.
 */
describe('the line a resolved vault call leaves', () => {
  it('names the call and the ending', () => {
    expect(vaultAskEndingLine('approved').text).toContain('approved from another view');
    expect(vaultAskEndingLine('sent_back').text).toContain('sent back with a note');
    expect(vaultAskEndingLine('declined').text).toContain('declined in another view');
    expect(vaultAskEndingLine('abandoned').text).toContain("vault call's asking session");
    for (const ending of ['approved', 'declined', undefined]) {
      expect(vaultAskEndingLine(ending).text, 'never the Slack or config line').not.toContain(
        'config write',
      );
      expect(vaultAskEndingLine(ending).text, 'never the Slack line').not.toContain('Slack');
    }
  });

  it('still says the call is gone for an ending this client is older than', () => {
    expect(vaultAskEndingLine({ something_new: null }).text).toContain('no longer waiting');
  });
});

/**
 * The read wait is the last hold: every state more specific than "the page is
 * still out" takes the body first, and two states take it instead.
 */
describe('what the read wait stands behind', () => {
  const running = seatRead();

  it('stands behind the command the reader sent', () => {
    expect(
      blocked(running, NO_COMPOSER, '/compact', true, false, true)?.line,
      'a page still loading covered the command in flight',
    ).toBe('Running /compact');
  });

  it('stands behind a machine turn', () => {
    const state = composerFrom({ take: null, notice: null, machine: 'handoff' });
    expect(blocked(running, state, null, true, false, true)?.line).toBe(
      'handing this session over',
    );
    const compacting = composerFrom({ take: null, notice: null, machine: 'compacting' });
    expect(blocked(running, compacting, null, true, false, true)?.line).toBe(
      'compacting this session',
    );
  });

  it('stands behind the seat states that have not come up', () => {
    expect(
      blocked(seatRead({ lifecycle: null, waking: true }), NO_COMPOSER, null, true, false, true)
        ?.line,
      'the read hold covered a wake',
    ).toBe('Waking up agent...');
    expect(
      blocked(seatRead({ lifecycle: null }), NO_COMPOSER, null, false, false, true)?.line,
      'the read hold covered a worker that is not running',
    ).toBe('not running');
    expect(
      blocked(
        seatRead({ lifecycle: 'Failed', reason: 'the CLI exited' }),
        NO_COMPOSER,
        null,
        true,
        false,
        true,
      )?.line,
      'the read hold covered the error',
    ).toBe('Input disabled due to error');
  });

  it('stands aside for a parked ask and for a live take', () => {
    expect(
      blocked(running, NO_COMPOSER, null, true, true, true),
      'the read hold covered the dock',
    ).toBeNull();
    const state = composerFrom({ take: take(), notice: null, machine: null });
    expect(
      blocked(running, state, null, true, false, true),
      'the read hold covered a take',
    ).toBeNull();
  });
});
