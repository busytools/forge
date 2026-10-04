import { describe, expect, it } from 'vitest';

import type { DictateWire } from '../wire/home';
import { dictationOffered, draftEndingLine } from './view';

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
    expect(draftEndingLine('expired').text, 'the window').toContain('expired unanswered');
    expect(draftEndingLine('abandoned').text, 'the asker').toContain('session went away');
    expect(
      draftEndingLine({ answered: { approved: true } }).text,
      'and whether the message went out',
    ).toContain('posted from another view');
    expect(
      draftEndingLine({ answered: { approved: false } }).text,
      'declined is its own line',
    ).toContain('declined in another view');
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
