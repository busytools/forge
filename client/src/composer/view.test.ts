import { describe, expect, it } from 'vitest';

import type { DictateWire } from '../wire/home';
import { dictationOffered } from './view';

/** The home's dictate read, with whatever the test overrides. */
function dictate(over: Partial<DictateWire> = {}): DictateWire {
  return {
    enabled: true,
    snapshot: { models: [], failure: null },
    models_dir: null,
    device: null,
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
  it('needs the section switched on AND a model loaded', () => {
    expect(
      dictationOffered(dictate({ snapshot: { models: [model], failure: null } })),
      'both',
    ).toBe(true);
    expect(
      dictationOffered(dictate({ enabled: false, snapshot: { models: [model], failure: null } })),
    ).toBe(false);
    expect(dictationOffered(dictate()), 'enabled with nothing loaded yet').toBe(false);
  });
});
