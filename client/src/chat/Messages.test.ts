import { render } from 'svelte/server';
import { describe, expect, it } from 'vitest';

import type { CallStatus } from './families';
import Messages from './Messages.svelte';
import type { MessageLane, PeerCard } from './units';

const card = (over: Partial<PeerCard> = {}): PeerCard => ({
  peer: 'forge/steward',
  body: 'picking up the render half now.',
  kind: 'message',
  here: true,
  org: null,
  status: 'completed',
  ...over,
});

const draw = (lanes: MessageLane[], status: CallStatus = 'completed'): string =>
  render(Messages, { props: { lanes, status } }).body;

describe('a run of peer messages, drawn as tool rows', () => {
  it('leads with the mark the group earned, not a constant check', () => {
    // The sheet's own sentence: the mark rolls the whole group up, "a failed
    // delivery included". A constant check draws a send that never arrived as
    // settled, and a send still out as settled too.
    const ask = (status: CallStatus): MessageLane[] => [
      { kind: 'ask', cards: [card({ kind: 'ask', status })] },
    ];

    const landed = draw(ask('completed'), 'completed');
    const failed = draw(ask('failed'), 'failed');
    const out = draw(ask('in_progress'), 'in_progress');

    expect(landed, 'a group whose sends landed').toContain('i-check');
    expect(failed, 'one with a send that did not arrive').toContain('i-x');
    expect(failed, 'in the failure tone').toContain('st err');
    expect(out, 'and one still out').toContain('ring');
    expect(out, 'is not drawn as settled').not.toContain('i-check');
  });

  it('counts the messages, and reads a lone one as the group of one it is', () => {
    // The one departure from the terminal, which holds a group back until it
    // holds two. The count is the whole message, singular included.
    const one = draw([{ kind: 'message', cards: [card()] }]);
    const two = draw([
      { kind: 'message', cards: [card(), card({ peer: 'gateway-backend', here: false })] },
    ]);

    expect(one, 'a lone message is one message, not a lone card').toContain('1 message<');
    expect(two).toContain('2 messages<');
  });

  it('draws a lane per kind, and the ask lane is the one that leads with the question mark', () => {
    const drawn = draw([
      { kind: 'ask', cards: [card({ kind: 'ask', peer: 'cli-version', here: false })] },
      { kind: 'reply', cards: [card({ kind: 'reply' })] },
      { kind: 'message', cards: [card()] },
    ]);

    // Two lanes share the inbound arrow because both are usually something
    // arriving; the lane word is what separates them.
    expect(drawn.match(/>ask</g)?.length, 'ask draws its own lane').toBe(1);
    expect(drawn, 'the ask lane leads with the question glyph').toContain('i-question');
    expect(drawn.match(/>message</g)?.length, 'and message its own').toBe(1);
    expect(drawn, 'the other two lanes carry the inbound arrow').toContain('i-in');
  });

  it('marks who is talking, and tags the org only when the card carries one', () => {
    const theirs = card({ peer: 'gateway-backend', here: false, org: 'Gateway' });
    const drawn = draw([{ kind: 'message', cards: [card(), theirs] }]);

    expect(drawn, 'a counterparty in this project').toContain('i-bot');
    expect(drawn, 'and one somewhere else').toContain('i-away');
    expect(drawn, 'the org the fold left on the card').toContain('>Gateway<');
    expect(
      drawn.match(/class="org"/g)?.length,
      'and no tag for the counterparty whose org is the reader own',
    ).toBe(1);
  });

  it('labels the message with its sender and its first line, and opens onto the body', () => {
    const body =
      'pgtemp, spawned per test binary rather than per test.\n\n' + 'The fixture is the example.';
    const drawn = draw([{ kind: 'message', cards: [card({ body })] }]);

    expect(drawn, 'the sender is the label the reader would have passed').toContain(
      '<span class="k">forge/steward</span>',
    );
    // The label, up to the chevron that closes the summary: a body sits in the
    // DOM whether or not the row is open, so the whole render cannot say what
    // the row previews.
    const at = drawn.indexOf('<span class="tn">');
    const label = drawn.slice(at, drawn.indexOf('<svg', at));
    expect(label, 'the label previews the first line').toContain(
      'pgtemp, spawned per test binary rather than per test.',
    );
    expect(label, 'and not the rest of the body').not.toContain('The fixture is the example.');
    expect(drawn.match(/<p>/g)?.length, 'blank lines break the body into paragraphs').toBe(2);
  });
});
