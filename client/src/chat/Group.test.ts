import { readFileSync } from 'node:fs';

import { render } from 'svelte/server';
import { describe, expect, it } from 'vitest';

import Group from './Group.svelte';
import type { Lane, MessageKind, PeerCard } from './units';

const card = (over: Partial<PeerCard> = {}): PeerCard => ({
  id: 't-1',
  peer: 'forge/steward',
  body: 'picking up the render half now.',
  kind: 'message',
  here: true,
  org: null,
  status: 'completed',
  ...over,
});

const lane = (kind: MessageKind, cards: PeerCard[]): Lane => ({ tag: 'message', kind, cards });

const draw = (lanes: Lane[]): string => render(Group, { props: { lanes } }).body;

describe('a lane of peer messages, drawn as tool rows', () => {
  it('draws a lone message as its lane and its card, with no summary over them', () => {
    // There is no count and no disclosure around a group: the lane's own row
    // and the message under it are the whole of what it draws.
    const one = draw([lane('message', [card()])]);

    expect(one, 'the lane drew').toContain('class="knd"');
    expect(one, 'and the card under it').toContain('picking up the render half now.');
  });

  it('draws a lane per kind, and the ask lane is the one that leads with the question mark', () => {
    const drawn = draw([
      lane('ask', [card({ kind: 'ask', peer: 'cli-version', here: false })]),
      lane('reply', [card({ kind: 'reply' })]),
      lane('message', [card()]),
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
    const drawn = draw([lane('message', [card(), theirs])]);

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
    const drawn = draw([lane('message', [card({ body })])]);

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

describe('the thinking lane', () => {
  const thought = (text: string): Lane => ({ tag: 'thought', thoughts: [{ key: 'a1#0', text }] });

  it('names the kind on the lane and previews the thought joined into one line', () => {
    const drawn = draw([thought('the first thing I checked\n\nand what came after it.')]);

    expect(drawn, 'the lane says what kind of thing this is').toContain('>thinking<');
    expect(drawn, 'the lane leads with the brain').toContain('href="#i-brain"');
    expect(drawn, 'and each row leads with its own bulb').toContain('href="#i-lightbulb"');
    // The row shows the text joined, not cut at its first newline: a thought's
    // first line is often a stub with the substance below it. The layout is
    // what breaks the line, at the width the row has.
    const at = drawn.indexOf('<span class="tn">');
    const label = drawn.slice(at, drawn.indexOf('<svg', at));
    expect(label, 'the preview carries the text joined').toContain(
      'the first thing I checked and what came after it.',
    );
    expect(label, 'with no newline left in it').not.toContain('\n');
  });

  it('renders the row line as markdown, the marks a single line can draw', () => {
    // The row showed its own asterisks and hashes before this; the line takes
    // emphasis and code, which are the marks that survive one line.
    const drawn = draw([thought('the **fold** joins, then `cargo check` runs')]);

    const at = drawn.indexOf('<span class="tn">');
    const label = drawn.slice(at, drawn.indexOf('<svg', at));
    expect(label, 'bold draws bold').toContain('<strong>fold</strong>');
    expect(label, 'and code draws code').toContain('<code>cargo check</code>');
    expect(label, 'with none of the marks shown raw').not.toContain('**');
  });

  it('draws the opened thought as markdown, which is how the model wrote it', () => {
    // The reasoning arrives with headings, lists and code, and drawn as plain
    // paragraphs it showed its own asterisks and hashes.
    const drawn = draw([thought('## what I found\n\n- one\n- two\n\n`cargo check`')]);

    expect(drawn, 'a heading is a heading').toContain('<h2>');
    expect(drawn, 'a list is a list').toContain('<li>');
    expect(drawn, 'and code keeps its own face').toContain('<code>');
  });

  it('sizes the row bulb below the marks, where its ink sits with the words', () => {
    // The bulb's ink nearly fills its box, so at the 15 the check wears its
    // base ran below the baseline and the row read low. Dropping the size rule
    // silently returns the row to that.
    const sheet = readFileSync(new URL('../assets/web.css', import.meta.url), 'utf8');
    expect(sheet, 'the bulb keeps its own size').toMatch(/\.st\.bulb\s*\{[^}]*13px/);
  });
});
