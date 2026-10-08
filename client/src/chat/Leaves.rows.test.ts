import { render } from 'svelte/server';
import { describe, expect, it } from 'vitest';

import Leaves from './Leaves.svelte';
import { leafOf } from './leaves';
import PeerRow from './PeerRow.svelte';
import type { PeerCard, WorkRow } from './units';

/** One peer card, as the row the leaf list draws. */
const card = (over: Partial<PeerCard> = {}): Extract<WorkRow, { tag: 'card' }> => ({
  tag: 'card',
  card: {
    id: 'm-1',
    row: 'arrived',
    peer: 'forge/steward',
    body: 'picking up the render half now.',
    org: null,
    status: 'completed',
    ack: null,
    seat: null,
    seats: [],
    ...over,
  },
});

const draw = (rows: WorkRow[]): string => render(Leaves, { props: { rows } }).body;

/** Card rows drawn the way their body needs them: open. */
const drawOpen = (cards: Partial<PeerCard>[]): string =>
  cards
    .map((over) => render(PeerRow, { props: { card: card(over).card, open: true } }).body)
    .join('\n');

describe('the rows of peer traffic, drawn as tool rows', () => {
  /**
   * A closed card carries its summary and nothing else: the summary's own
   * line is `firstLine`, and everything after it is the body's. The giant
   * seat's cost is the bodies of rows nobody opened.
   */
  it('carries summary markup only while it is closed', () => {
    const two = card({ body: 'the first line rides the summary\nand this one is the body alone' });
    const closed = draw([two]);
    const under = closed.slice(closed.indexOf('</summary>'));
    expect(closed, 'the summary line still draws').toContain('the first line');
    expect(under, 'and nothing under it').not.toContain('body alone');
  });

  it('draws a card as its own row, with nothing above it', () => {
    // There is no count and no disclosure around the list - and no family row
    // either: the rows are the whole of what it draws.
    const one = draw([card()]);

    expect(one, 'the card drew').toContain('picking up the render half now.');
  });

  it('draws every card the list holds, whatever direction it went', () => {
    const drawn = draw([
      card({ row: 'sent', peer: 'forge/steward' }),
      card({ id: 'm-2', row: 'arrived', peer: 'gateway-backend', org: 'Granite' }),
      card({ id: 'm-3', row: 'whoami', peer: 'whoami' }),
      card({ id: 'm-4', row: 'list', peer: 'list' }),
    ]);

    expect((drawn.match(/<details class="leaf"/g) ?? []).length, 'one row each').toBe(4);
  });

  it('carries the direction in the words, and the mark reinforces it', () => {
    const drawn = draw([
      card({ row: 'sent' }),
      card({ id: 'm-2', row: 'arrived', peer: 'gateway-backend' }),
    ]);

    expect(drawn, 'the outgoing row says which way it went').toContain('>sent to</span>');
    expect(drawn, 'and carries the plane').toContain('href="#i-plane"');
    expect(drawn, 'the arriving row says where it came from').toContain('>from</span>');
    expect(drawn, 'and carries the inbox').toContain('href="#i-inbox"');
  });

  it('tags the org only when the counterparty is outside the reader own', () => {
    const drawn = draw([card(), card({ id: 'm-2', peer: 'gateway-backend', org: 'Gateway' })]);

    expect(drawn, 'the org the fold left on the card').toContain('>Gateway<');
    expect(
      drawn.match(/class="org"/g)?.length,
      'and no tag for the counterparty whose org is the reader own',
    ).toBe(1);
  });

  it('labels the message with its sender and its first line, and opens onto the body', () => {
    const body =
      'pgtemp, spawned per test binary rather than per test.\n\n' + 'The fixture is the example.';
    const drawn = drawOpen([{ body }]);

    expect(drawn, 'the sender is the label the reader would have passed').toContain(
      '<span class="k">forge/steward</span>',
    );
    // The label, up to the chevron that closes the summary: the row is drawn
    // open, so its body is in this render too and the whole of it cannot say
    // what the summary previews.
    const at = drawn.indexOf('<span class="tn"');
    const label = drawn.slice(at, drawn.indexOf('<svg', at));
    expect(label, 'the label previews the first line').toContain(
      'pgtemp, spawned per test binary rather than per test.',
    );
    expect(label, 'and not the rest of the body').not.toContain('The fixture is the example.');
    expect(drawn.match(/<p>/g)?.length, 'blank lines break the body into paragraphs').toBe(2);
  });

  it('opens a send onto the ack its own result carried', () => {
    const drawn = drawOpen([{ row: 'sent', ack: 'id m-7f3a92e0 · to Busytools/forge/w1' }]);

    expect(drawn, 'the ack is on the row').toContain('id m-7f3a92e0');
    expect(drawn, 'and names the seat it went to').toContain('to Busytools/forge/w1');
  });

  it('draws a failed delivery as the outgoing row, warn-toned', () => {
    const drawn = draw([
      card({ row: 'failed', peer: 'companies', body: 'channel closed', status: 'failed' }),
    ]);

    expect(drawn, 'the row says what happened, in the words').toContain(
      'failed to deliver: channel closed',
    );
    expect(drawn, 'and keeps the outgoing mark').toContain('href="#i-plane"');
    expect(drawn, 'with the warning tone on the title').toContain('class="tn warn"');
  });

  it('opens whoami onto this seat and list onto the seats it can reach', () => {
    const drawn = drawOpen([
      {
        row: 'whoami',
        peer: 'whoami',
        seat: {
          org: 'Busytools',
          project: 'forge',
          label: 'lead',
          path: '/tmp/forge',
          status: 'running',
        },
      },
      {
        id: 'm-2',
        row: 'list',
        peer: 'list',
        seats: [
          {
            org: 'Busytools',
            label: 'lead',
            project: 'forge',
            what: 'this session',
            liveness: '',
          },
          {
            org: 'Busytools',
            label: 'w1',
            project: 'forge',
            what: 'review the diff',
            liveness: 'idle',
          },
          {
            org: 'Gateway',
            label: 'lead',
            project: 'gateway-backend',
            what: 'another project',
            liveness: '',
          },
        ],
      },
    ]);

    expect(drawn, 'whoami says what it is').toContain('whoami');
    expect(drawn, 'and opens onto the seat').toContain('>Busytools<');
    expect(drawn, 'its path').toContain('/tmp/forge');
    expect(drawn, 'the list row names each seat').toContain('>w1<');
    expect(drawn, 'with what it is for').toContain('review the diff');
    expect(drawn, "its activity beside it, in the wire's own lower case").toContain('idle');
    expect(drawn, "the reader's own seat says so").toContain('this session');
    expect(drawn, "and a project that is not the reader's says that").toContain('another project');
  });

  it('draws an agent row as two facts and a worker row as three', () => {
    // A project's own agent has no activity to report, so its row carries the
    // seat and what it is for - and a row that printed the third clause
    // unconditionally ended in a dangling separator.
    const drawn = drawOpen([
      {
        id: 'm-1',
        row: 'list',
        peer: 'list',
        seats: [
          {
            org: 'Busytools',
            label: 'lead',
            project: 'forge',
            what: 'this session',
            liveness: '',
          },
          {
            org: 'Busytools',
            label: 'w1',
            project: 'forge',
            what: 'review the diff',
            liveness: 'idle',
          },
        ],
      },
    ]);

    const values = [...drawn.matchAll(/<span class="v">(.*?)<\/span>/g)].map(
      (match) => match[1] ?? '',
    );
    expect(values[0], "the agent's row is its project and what it is for").toContain(
      'forge \u{b7} this session',
    );
    expect(values[0], 'with nothing dangling after it').not.toContain('this session \u{b7}');
    expect(values[1], "and the worker's keeps its activity").toContain(
      'forge \u{b7} review the diff \u{b7} idle',
    );
  });

  it('draws a failure with no reason as the row alone, and never the message sent', () => {
    // A refusal that carried no words has nothing to put under the row, and
    // falling back to the sent text would say the words arrived. Pinned here
    // because the fold's own test does not draw.
    const drawn = draw([
      card({
        id: 'm-1',
        row: 'failed',
        peer: 'companies',
        body: '',
        status: 'failed',
      }),
    ]);

    const at = drawn.indexOf('<span class="tn');
    const title = drawn.slice(at, drawn.indexOf('<svg', at));
    expect(title, 'the row says what happened').toContain('failed to deliver:');
    expect(title, 'and carries no message under it').not.toContain('picking it up');
  });

  /**
   * **Every project's own agent is labelled `lead`**, so the ordinary
   * unfiltered `list` - the documented health check - carries two rows with
   * that word. Their key is asserted where the refusal lives: a duplicate key
   * is the client runtime's, so `Leaves.mount.test.ts` is the file that throws
   * for it. This one pins what the rows draw.
   */
  it('draws a list whose rows share the word lead', () => {
    const drawn = drawOpen([
      {
        id: 'm-1',
        row: 'list',
        peer: 'list',
        seats: [
          {
            org: 'Busytools',
            label: 'lead',
            project: 'forge',
            what: 'this session',
            liveness: '',
          },
          {
            org: 'Gateway',
            label: 'lead',
            project: 'gateway-backend',
            what: 'another project',
            liveness: '',
          },
        ],
      },
    ]);

    expect(drawn.match(/>lead</g)?.length, 'both rows drew').toBe(2);
    expect(drawn, 'under their own projects').toContain('gateway-backend');
  });

  /**
   * **A peer message is prose from another session**, so its body reads the way
   * every other message's does: the marks render rather than sitting on the
   * page as themselves, and the row's own preview renders them too (Ved,
   * 2026-10-03).
   */
  it('draws a peer body as prose, not as the raw text it arrived as', () => {
    const drawn = drawOpen([{ body: 'the **fold** joins, `cargo check` runs' }]);
    expect(drawn, 'the marks render').toContain('<strong>fold</strong>');
    expect(drawn, 'rather than sitting on the page').not.toContain('**fold**');
  });
});

describe('a systemone decision row', () => {
  it('opens the decided call, which holds only while the list hands its decision down', () => {
    // The opener is the LIST's call site: this row draws open because
    // `Leaves.svelte` passes the leaf's decision into `opensByDefault`, and a
    // list handing `null` instead would silently revert to a closed row.
    const leaf = leafOf(
      'tu-dec',
      'mcp__forge__systemone__ask_noul',
      { state: 'a one-line import fix', instructions: 'Is this mechanical?' },
      {
        type: 'tool_result',
        content: '{"model":"jev-1.13.0","answer":{"type":"noul","noul":0.93}}',
      },
    );
    const drawn = draw([{ tag: 'call', key: 'c-tu-dec', leaf }]);

    expect(drawn, 'the row leads with the fork').toContain('href="#i-decide"');
    const tag = /<details[^>]*>/.exec(drawn)?.[0] ?? '';
    expect(tag, 'the row draws open').toContain('open');
    expect(drawn, 'with the block inside it').toContain('class="dec"');
  });
});

describe('the thinking row', () => {
  const thought = (text: string): WorkRow => ({ tag: 'thought', key: 'a1#0', text });

  it('leads with the bubble and previews the thought joined into one line', () => {
    const drawn = draw([thought('the first thing I checked\n\nand what came after it.')]);

    expect(drawn, 'the row leads with the bubble').toContain('href="#i-message-circle-more"');
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
});
