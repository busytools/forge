import { readFileSync } from 'node:fs';

import { render } from 'svelte/server';
import { describe, expect, it } from 'vitest';

import type { Turn as HeldTurn } from './conversation';
import Turn from './Turn.svelte';

/** A turn holding `messages`, drawn as the page draws it. */
const draw = (...messages: unknown[]): string =>
  render(Turn, { props: { turn: { key: 't1', messages, live: false } as HeldTurn, cwd: null } })
    .body;

/** The same turn while its frames are still arriving. */
const live = (...messages: unknown[]): string =>
  render(Turn, { props: { turn: { key: 't1', messages, live: true } as HeldTurn, cwd: null } })
    .body;

/** The same turn, while a compaction is in flight. */
const compacting = (...messages: unknown[]): string =>
  render(Turn, {
    props: { turn: { key: 't1', messages, live: false } as HeldTurn, cwd: null, compacting: true },
  }).body;

/** The same turn as a page carried it, while the seat says a turn is running. */
const seatRunning = (...messages: unknown[]): string =>
  render(Turn, {
    props: { turn: { key: 't1', messages, live: false, running: true } as HeldTurn, cwd: null },
  }).body;

/** One assistant message carrying prose and the counters of its own call. */
const working = {
  type: 'assistant',
  uuid: 'a1',
  timestamp: '2026-10-01T06:00:00Z',
  message: {
    id: 'm1',
    role: 'assistant',
    model: 'claude-opus-5',
    content: [{ type: 'text', text: 'working' }],
    usage: {
      input_tokens: 100,
      output_tokens: 20,
      cache_read_input_tokens: 1000,
      cache_creation_input_tokens: 0,
    },
  },
};

const prompt = (text: string): unknown => ({
  type: 'user',
  message: { role: 'user', content: [{ type: 'text', text }] },
  uuid: 'u1',
});

const said = (content: unknown[], id = 'm1'): unknown => ({
  type: 'assistant',
  message: { id, role: 'assistant', model: 'claude-opus-5', content },
});

const use = (id: string, name: string, input: unknown): unknown => ({
  type: 'tool_use',
  id,
  name,
  input,
});

const result = (id: string, value: string): unknown => ({
  type: 'user',
  message: { role: 'user', content: [{ type: 'tool_result', tool_use_id: id, content: value }] },
  uuid: `r-${id}`,
});

/** Everything between two tags, so an assertion reads a row rather than a page. */
const between = (body: string, from: string, to: string): string => {
  const start = body.indexOf(from);
  const end = body.indexOf(to, start + 1);
  return start === -1 ? '' : body.slice(start, end === -1 ? undefined : end);
};

describe('one turn, as the page draws it', () => {
  it('draws what the reader said, and the work under it', () => {
    const body = draw(
      prompt('run the gate'),
      // The call rides inside the assistant's own content, which is the shape
      // the wire uses: a message is a frame, and its blocks are what it said
      // and what it called.
      said([
        { type: 'text', text: 'running it now' },
        use('c1', 'Bash', { command: 'just check' }),
      ]),
      result('c1', 'all green'),
    );

    // The reader's turn carries no label: the rule beside it is the
    // attribution, which is the design's own decision rather than a missing
    // word.
    const mine = between(body, '<div class="mine"', '</div>');
    expect(mine).toContain('run the gate');
    expect(mine, 'and nothing in it names the speaker').not.toMatch(/you|user|prompt/i);
    expect(
      between(body, '<div class="work">', '<details'),
      'the work block draws the prose',
    ).toContain('running it now');
    expect(body).toContain('just check');
  });

  it('draws what the reader typed as the document the terminal draws', () => {
    // The terminal sends a user block through the same markdown path it sends
    // an assistant's (`message.rs`'s `block_markdown_for`), so a heading someone
    // types arrives as a heading and a fence as the page's own code panel. The
    // issue names the shapes where the two readings diverge most - a fence, an
    // inline span, and a fence that never closes, which in a prompt is normal
    // rather than an error.
    const body = draw(prompt('Run this:\n\n```sh\njust check\n```\n\nand read `out`.'));

    expect(body, 'a fence in what the reader typed draws as the code panel').toContain(
      'class="code"',
    );
    expect(body, 'and an inline span draws as a code span').toContain('<code>out</code>');
    expect(body, 'and the words are still there').toContain('just check');
  });

  it('keeps the newlines a reader typed, which assistant prose does not', () => {
    // The terminal's split: a user block goes through the same path with
    // `preserve_newlines`, an assistant block without it. A prompt is usually
    // several lines, so this is the shape a person meets first.
    const typed = draw(prompt('first line\nsecond line'));
    const answered = draw(said([{ type: 'text', text: 'first line\nsecond line' }]));

    expect(typed, 'the break the reader typed survives').toContain('<br>');
    expect(answered, 'and the assistant prose still joins').not.toContain('<br>');
  });

  it('draws what the model thought as a collapsed row carrying its own words', () => {
    // The terminal does not render thinking text at all - its arm sets a status
    // and traces a count - so this is the client beyond it rather than beside
    // it, in the terminal's own collapsed vocabulary: the row carries the first
    // of its words, and the whole of it is behind the row's open.
    const body = draw(
      said([
        {
          type: 'thinking',
          thinking: 'first the model wondered\nand then it kept going',
          signature: 'sig',
        },
      ]),
    );

    // The summary alone, because the words are in the body too: an assertion on
    // the whole render passes whether or not the row carries them.
    const at = body.indexOf('<span class="tn">');
    const summary = body.slice(at, body.indexOf('</summary>', at));
    expect(summary, 'the row leads with the thinking own first words').toContain(
      'first the model wondered',
    );
    expect(summary, 'and only that line of it').not.toContain('and then it kept going');
    expect(summary, 'and carries the shared disclosure chevron').toContain('#i-chev');
    expect(body, 'with the whole of it inside').toContain('and then it kept going');
  });

  it('draws a running row for a turn the frames built and no result has settled', () => {
    // The wiring this rides on is one line: `turn.live` reaching the fold. It
    // cannot be inferred from the frames - a saved page carries no result
    // frame either - so the caller's fact is the only carrier, and this pins
    // that it is passed.
    const running = between(live(working), '<details class="turninfo"', '</details>');
    expect(running, 'a ring, not a settled check').toContain('class="ring"');
    expect(running, 'the figures the frames carry').toContain('100\u{2191}');
    expect(running, 'the thinking count beside them').toContain('thinking');
    expect(running, 'and no cost segment for a figure no frame has carried yet').not.toContain(
      'cumulative',
    );

    expect(draw(working), 'the same frames read as a page draw no row at all').not.toContain(
      'turninfo',
    );
  });

  it('draws no running row for a turn whose row is pinned above the box', () => {
    // While this turn is the one being written its running row is drawn in the
    // strip above the composer, outside the scroll - so the turn does not draw
    // a second copy of it: one strip, never two. The settled row is untouched,
    // which is the half that makes this a move rather than a loss.
    const at = (turn: HeldTurn): string =>
      render(Turn, { props: { turn, cwd: null, pinned: true } }).body;

    expect(
      at({ key: 't1', messages: [working], live: true }),
      'a running turn the frames built',
    ).not.toContain('turninfo');
    expect(
      at({ key: 't1', messages: [working], live: false, running: true }),
      'and one whose turn the seat says is running',
    ).not.toContain('turninfo');
    expect(
      at({
        key: 't1',
        live: false,
        messages: [
          working,
          { type: 'result', uuid: 'r1', duration_ms: 1000, duration_api_ms: 500, usage: {} },
        ],
      }),
      'while the settled row still draws where it always did',
    ).toContain('turninfo');
  });

  it('draws the running row for a turn the seat says is running', () => {
    // The other carrier of the same fact, and it is a different one: a turn the
    // client reached mid-flight has its row from a page, so `running` is what
    // reaches the fold - and a live turn written over it would be one turn in
    // two. The row draws the bar from it exactly as it does for `live`.
    const running = between(seatRunning(working), '<details class="turninfo"', '</details>');
    expect(running, 'a ring, not a settled check').toContain('class="ring"');
    expect(running, 'the figures the frames carry').toContain('100\u{2191}');
    expect(running, 'the thinking count beside them').toContain('thinking');
  });

  it('draws the runs the fold cut, rather than regrouping what it holds', () => {
    // A question splits a run, so this turn holds TWO groups with a card
    // between them. A component that grouped its own rows instead of drawing
    // the fold's would merge them into one - and every other test here passes
    // either way, which is what makes this the one that pins it.
    const body = draw(
      said([
        use('c1', 'Read', { file_path: 'a.rs' }),
        use('q1', 'AskUserQuestion', {
          questions: [{ question: 'Which one?', options: [{ label: 'a' }] }],
        }),
        use('c2', 'Read', { file_path: 'b.rs' }),
      ]),
    );

    const groups = body.match(/<details class="kind"/g) ?? [];
    expect(groups, 'two runs, so two groups').toHaveLength(2);
    expect(body).toContain('<div class="card">');
    expect(body, 'and the first run keeps its own one call').toContain('1 tool call');
  });

  it('draws a search hit as a location and the line beneath it', () => {
    // Two elements. As one run with a newline character in it the pair drew as
    // a single line with the path run into the matched text, because nothing
    // in this box is pre-formatted.
    const body = draw(
      said([use('c1', 'Grep', { pattern: 'render_group_summary' })]),
      result('c1', 'crates/forge-server/src/grouping.rs:142:render_group_summary(unit, width)'),
    );

    expect(body).toContain('<div class="searchhit">');
    expect(body).toContain('<div class="where"><span class="ln">142:</span> <span class="fl">');
    expect(body).toContain('<div class="src">render_group_summary(unit, width)</div>');
  });

  it('names what a turn attached, and draws none of its pixels', () => {
    const attached = {
      type: 'user',
      message: {
        role: 'user',
        content: [
          { type: 'text', text: 'what is wrong with this layout?' },
          {
            type: 'image',
            source: {
              type: 'base64',
              media_type: 'image/png',
              data: 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0C8AAAAASUVORK5CYII=',
            },
          },
        ],
      },
      uuid: 'u2',
    };

    const body = draw(attached);
    // The words are read inside the turn's own block; what came with them draws
    // after the prose, so its name and its size are asserted on the page.
    const mine = between(body, '<div class="mine"', '<div class="attrow"');
    expect(mine, 'the words are still the turn').toContain('what is wrong with this layout?');
    expect(body, 'and what came with them is named').toContain('image/png');
    expect(body, 'with the size the payload states').toContain('68 B');
    expect(body, 'and no bytes in the page').not.toContain('iVBORw0KGgo');
  });

  it('draws no size for an attachment the wire gave as a url', () => {
    const linked = {
      type: 'user',
      message: {
        role: 'user',
        content: [
          { type: 'text', text: 'this one is a link' },
          {
            type: 'image',
            source: { type: 'url', media_type: 'image/png', url: 'https://example.test/a.png' },
          },
        ],
      },
      uuid: 'u3',
    };

    const body = draw(linked);
    expect(body, 'the name the wire gave it').toContain('image/png');
    // A payload with no length has no size, and the span that would hold one
    // draws a bare dash in its place.
    expect(body, 'and no size beside it').not.toContain('<span class="n">');
  });

  it('draws the compaction line only while one is in flight', () => {
    // The line is the whole of what a reader watching a 43-second compaction
    // has to go on: the conversation is otherwise silent for the length of it.
    expect(
      compacting(said([{ type: 'text', text: 'Folding the earlier context down first.' }])),
    ).toContain('Compacting context');
    expect(draw(said([{ type: 'text', text: 'And this one is done.' }]))).not.toContain(
      'Compacting context',
    );
  });

  it('puts the compaction line under the work, and out of the reader block', () => {
    const body = compacting(
      prompt('compact it and carry on'),
      said([{ type: 'text', text: 'Folding the earlier context down first.' }]),
    );

    // The last block is the work, so the line closes it rather than opening a
    // second one - and it comes after what the turn did.
    expect(body.match(/<div class="work">/g) ?? []).toHaveLength(1);
    const work = body.slice(body.indexOf('<div class="work">'));
    expect(work.indexOf('Folding the earlier context down first.')).toBeGreaterThanOrEqual(0);
    expect(work.indexOf('Compacting context')).toBeGreaterThan(
      work.indexOf('Folding the earlier context down first.'),
    );

    // A turn with more than one run of work keeps it in the last: the line is
    // the end of the turn, not the end of every block on it.
    const twice = compacting(
      prompt('first'),
      said([{ type: 'text', text: 'one' }]),
      prompt('second'),
      said([{ type: 'text', text: 'two' }]),
    );
    expect(twice.match(/Compacting context/g) ?? []).toHaveLength(1);

    // And a turn ending on the reader's own words keeps it out of their
    // attribution, which is what the orange rule on that block is for.
    const spoken = compacting(prompt('compact it and carry on'));
    expect(between(spoken, '<div class="mine"', '</div>')).not.toContain('Compacting context');
    expect(spoken).toContain('Compacting context');
  });

  it('draws the compaction line above the turn footer, not below it', () => {
    // The footer is the hooks chip and the report row, and the line sits above
    // them - the order the terminal settled, and the one the book's page and
    // the approved mockup both draw.
    const body = compacting(
      said([{ type: 'text', text: 'Folding the earlier context down first.' }]),
      {
        type: 'system',
        subtype: 'stop_hook_summary',
        hookCount: 1,
        hookInfos: [],
        uuid: 'hooks-1',
      },
      { type: 'result', uuid: 'r1', duration_ms: 1000, duration_api_ms: 500, usage: {} },
    );

    const at = (marker: string): number => body.indexOf(marker);
    expect(at('Compacting context'), 'the line is drawn').toBeGreaterThanOrEqual(0);
    expect(at('Compacting context'), 'and above the hooks chip').toBeLessThan(at('hook summary'));
    expect(at('Compacting context'), 'and above the report row').toBeLessThan(at('turninfo'));
  });

  it('draws a mutation with its diff already open', () => {
    const body = draw(
      said([
        use('c1', 'Edit', {
          file_path: 'crates/forge-web/src/home.css',
          old_string: 'flex: none;',
          new_string: 'flex: 0 1 auto;',
        }),
      ]),
    );

    expect(
      between(body, '<details', '</details>'),
      'a mutation opens without being asked',
    ).toContain('open');
    expect(body).toContain('class="ln d"');
    expect(body).toContain('class="ln a"');
  });

  it('leaves a call that is still out closed, with the ring for a status', () => {
    const body = draw(said([use('c1', 'Bash', { command: 'just check' })]));

    // The GROUP opens - the mockup draws a run open - and the call inside it
    // waits to be asked, which is the difference from the terminal: it expands
    // everything at once and the page opens one call at a time.
    const leaf = body.slice(body.indexOf('<details class="leaf"'));
    expect(leaf, 'the call is on the page').not.toBe('');
    expect(leaf.slice(0, leaf.indexOf('>')), 'it waits to be asked').not.toContain('open');
    expect(leaf).toContain('<span class="ring"></span>');
  });

  it('marks a message by the seat the page is drawing, not by the row alone', () => {
    // The mark says whether the counterparty is in THIS project, so the row
    // cannot decide it: the seat has to reach the fold. A turn drawn with no
    // seat in hand takes the ordinary case - in this project - rather than
    // guessing at a stranger.
    const envelope = (who: string): unknown => ({
      type: 'user',
      message: {
        role: 'user',
        content: [
          { type: 'text', text: `[Message id=t-1 from agent '${who}' (org 'Busytools')]\n\nhi` },
        ],
      },
      uuid: 'u1',
    });
    const seat = { org: 'Busytools', project: 'forge', label: 'chat-kinds' };

    const seated = render(Turn, {
      props: {
        turn: { key: 't1', messages: [envelope('gateway-backend')], live: false } as HeldTurn,
        cwd: null,
        slot: seat,
      },
    }).body;
    const unseated = draw(envelope('gateway-backend'));

    expect(seated, 'another project draws away').toContain('i-away');
    expect(unseated, 'and with no seat to compare against, the ordinary case').toContain('i-bot');
  });
});

describe('the block the reader typed', () => {
  it('is marked by its line alone, with no wash behind it', () => {
    // The wash duplicated the line, and over a turn of several user messages
    // the column read as banded rather than as marked. Read off the sheet:
    // jsdom performs no layout, so nothing rendered can see a background.
    const sheet = readFileSync(new URL('../assets/web.css', import.meta.url), 'utf8');
    const at = sheet.indexOf('.mine {');
    expect(at, '.mine is in the sheet').toBeGreaterThan(-1);
    const mine = sheet.slice(at, sheet.indexOf('}', at));
    expect(mine, 'the line is the mark').toContain('border-left: 1px solid var(--accent)');
    expect(mine, 'and nothing washes the block behind it').not.toContain('background');
  });
});
