import { readFileSync } from 'node:fs';

import { render } from 'svelte/server';
import { describe, expect, it } from 'vitest';

import { beingWritten, type Turn as HeldTurn } from './conversation';
import Turn from './Turn.svelte';
import { fold } from './units';

/** A turn holding `messages`, drawn as the page draws it. */
const draw = (...messages: unknown[]): string =>
  render(Turn, { props: { turn: { key: 't1', messages, live: false } as HeldTurn } }).body;

/** The same turn while its frames are still arriving. */
const live = (...messages: unknown[]): string =>
  render(Turn, { props: { turn: { key: 't1', messages, live: true } as HeldTurn } }).body;

/** The same turn as a page carried it, while the seat says a turn is running. */
const seatRunning = (...messages: unknown[]): string =>
  render(Turn, {
    props: { turn: { key: 't1', messages, live: false, running: true } as HeldTurn },
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

const result = (id: string, value: string, record?: unknown): unknown => ({
  type: 'user',
  message: { role: 'user', content: [{ type: 'tool_result', tool_use_id: id, content: value }] },
  uuid: `r-${id}`,
  ...(record === undefined ? {} : { tool_use_result: record }),
});

/** The frame a turn ends on, which is the only carrier of a session cost. */
const ended = (uuid: string, total: number): unknown => ({
  type: 'result',
  uuid,
  duration_ms: 1000,
  duration_api_ms: 500,
  total_cost_usd: total,
  usage: {},
});

/**
 * The key of each report row a fold draws for this turn, oldest first.
 *
 * Asked of the fold rather than written out, because the key is the fold's own
 * - a fixture naming one would be asserting the name rather than the row.
 */
const reportKeys = (turn: HeldTurn): string[] =>
  fold(turn.messages, null, beingWritten(turn))
    .filter((unit) => unit.kind === 'report')
    .map((unit) => unit.key);

/** Everything between two tags, so an assertion reads a row rather than a page. */
const between = (body: string, from: string, to: string): string => {
  const start = body.indexOf(from);
  const end = body.indexOf(to, start + 1);
  return start === -1 ? '' : body.slice(start, end === -1 ? undefined : end);
};

describe('one turn, as the page draws it', () => {
  /**
   * **Every row carries the fold's own key**, which is what the column's anchor
   * finds the reader's row by when the layout moves under them
   * (`chat/anchor.ts`): a row measures taller as its stretch is corrected, and
   * without a key on the row there is nothing to put the reader back on.
   */
  it("carries each unit's key on the row, for the column's anchor", () => {
    const whose = {
      type: 'user',
      uuid: 'u1',
      message: { role: 'user', content: [{ type: 'text', text: 'what is this' }] },
    };
    const body = draw(whose, said([{ type: 'text', text: 'one line of prose' }]));

    // Both attributes on the same div, in whichever order the renderer writes
    // them: the order is not this test's business, the pairing is.
    const carrying = (name: string): RegExp =>
      new RegExp(`<div\\b(?=[^>]*\\bclass="${name}")(?=[^>]*\\bdata-k="[^"]+")[^>]*>`);

    expect(body, "the reader's own row carries its key").toMatch(carrying('mine'));
    expect(body, 'and the work rows theirs').toMatch(carrying('unit'));
    // **And each carries its turn's key too**: the fold names an id-less frame
    // within its own turn, so two turns can carry the same unit key and an
    // unprefixed lookup could hold a row of the wrong one.
    expect(body, "the row's key is its turn's").toContain('data-k="t1:');
  });

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

  it('draws what the model thought as a collapsed row carrying its words joined', () => {
    // The terminal does not render thinking text at all - its arm sets a status
    // and traces a count - so this is the client beyond it rather than beside
    // it, in the terminal's own collapsed vocabulary: the row carries the
    // thought joined into one line, and the layout breaks it at the row's
    // width, where a reader expects the row to end.
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
    expect(summary, 'the row leads with the thought joined, not cut at its newline').toContain(
      'first the model wondered and then it kept going',
    );
    expect(summary, 'and carries the shared disclosure chevron').toContain('#i-chev');
    expect(body, 'with the whole of it inside').toContain('and then it kept going');
  });

  it("draws a hook's own run as the row the fold made, closed on its first line", () => {
    // The wiring is one line in this component's arm list, and nothing else
    // here covers it: the row's own tests render it directly, and axe sees it
    // through the page rather than through this turn.
    const body = draw(
      {
        type: 'system',
        subtype: 'hook_started',
        hook_id: 'h1',
        hook_name: 'SessionStart:startup',
        hook_event: 'SessionStart',
        uuid: 'h1',
      },
      {
        type: 'system',
        subtype: 'hook_response',
        hook_id: 'h1',
        hook_name: 'SessionStart:startup',
        hook_event: 'SessionStart',
        output: 'repository is clean',
        stdout: 'repository is clean',
        stderr: '',
        exit_code: 0,
        outcome: 'success',
        uuid: 'h2',
      },
    );

    expect(body, 'the run is a row of the turn rather than a drop').toContain(
      'class="leaf hookrow"',
    );
    // The summary alone, because the output is in the body too: an assertion on
    // the whole render passes whether or not the first line reached the row.
    const at = body.indexOf('<summary');
    const summary = body.slice(at, body.indexOf('</summary>', at));
    expect(summary, 'the hook it matched, without opening it').toContain('SessionStart:startup');
    expect(summary, "with the output's first line on the closed row").toContain(
      'repository is clean',
    );
    expect(body, 'and the whole of what it printed behind the row').toContain(
      'repository is clean',
    );
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

  it('draws no report row for a turn the pin is holding, and takes it back when the pin lets go', () => {
    // The pin holds this turn's row - the running one while the turn is written
    // and the finished one for the beat after it ends - so the turn draws none
    // of it: the same figures twice on one page is the defect. The settled row
    // comes back here the moment the pin lets go, which is the half that makes
    // this a move rather than a loss.
    const at = (turn: HeldTurn, carried: string | null): string =>
      render(Turn, { props: { turn, carried } }).body;
    const settled: HeldTurn = {
      key: 't1',
      live: false,
      messages: [working, ended('r-1', 12.5)],
    };
    const [held] = reportKeys(settled);

    const writing: HeldTurn = { key: 't1', messages: [working], live: true };
    const seated: HeldTurn = { key: 't1', messages: [working], live: false, running: true };

    expect(
      at(writing, reportKeys(writing)[0] ?? null),
      'a running turn the frames built',
    ).not.toContain('turninfo');
    expect(
      at(seated, reportKeys(seated)[0] ?? null),
      'and one whose turn the seat says is running',
    ).not.toContain('turninfo');
    expect(at(settled, held ?? null), 'nor the finished row while the pin holds it').not.toContain(
      'turninfo',
    );
    expect(at(settled, null), 'and the turn takes it back when the pin lets go').toContain(
      'turninfo',
    );
  });

  it('drops the row the pin holds, and no other row the turn carries', () => {
    // **The pin holds one row, so the turn stands aside for one.** A turn can
    // carry two report rows - one per Result that landed in it, which is what a
    // refused ask or a queued prompt that never arrived as a frame leaves - and
    // a filter that dropped every report row would lose the row the pin never
    // took, for as long as the pin holds the one it did.
    const at = (turn: HeldTurn, carried: string | null): string =>
      render(Turn, { props: { turn, carried } }).body;
    const rows = (body: string): number => (body.match(/details class="turninfo"/g) ?? []).length;
    const twice: HeldTurn = {
      key: 't1',
      live: false,
      messages: [
        said([{ type: 'text', text: 'one' }], 'm1'),
        ended('r-1', 12.5),
        said([{ type: 'text', text: 'two' }], 'm2'),
        ended('r-2', 9),
      ],
    };
    const keys = reportKeys(twice);

    expect(keys, 'the turn carries a row per Result').toHaveLength(2);
    expect(rows(at(twice, null)), 'with the pin holding none, both draw').toBe(2);
    expect(rows(at(twice, keys[1] ?? '')), 'and with one held, the other still draws').toBe(1);
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
    // An ANSWERED question splits a run, so this turn holds TWO groups with a
    // card between them. A component that grouped its own rows instead of
    // drawing the fold's would merge them into one - and every other test here
    // passes either way, which is what makes this the one that pins it.
    const body = draw(
      said([
        use('c1', 'Read', { file_path: 'a.rs' }),
        use('q1', 'AskUserQuestion', {
          questions: [{ question: 'Which one?', options: [{ label: 'a' }] }],
        }),
        use('c2', 'Read', { file_path: 'b.rs' }),
      ]),
      result('q1', 'answered', { answers: { 'Which one?': 'a' } }),
    );

    const runs = body.match(/<div class="leaves"/g) ?? [];
    expect(runs, 'two runs, so two leaf lists').toHaveLength(2);
    expect(body).toContain('<div class="card">');
    expect(body, 'and each run keeps its own call').toContain('a.rs');
    expect(body, 'and the other its own').toContain('b.rs');
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

  it('draws a hook run that landed after the result in the group', () => {
    // A Stop hook's frames arrive at the turn's end, after the result that
    // settled it. The run is work the turn did, so it rides the rows like any
    // other work the turn did.
    const body = draw(
      said([{ type: 'text', text: 'Folding the earlier context down first.' }]),
      {
        type: 'system',
        subtype: 'stop_hook_summary',
        hookCount: 1,
        hookInfos: [],
        uuid: 'hooks-1',
      },
      { type: 'result', uuid: 'r1', duration_ms: 1000, duration_api_ms: 500, usage: {} },
      {
        type: 'system',
        subtype: 'hook_started',
        hook_id: 'h1',
        hook_name: 'Stop:check',
        hook_event: 'Stop',
        uuid: 'h1',
      },
    );

    const at = (marker: string): number => body.indexOf(marker);
    expect(
      at('class="leaf hookrow"'),
      'the run drew, in the group it belongs to',
    ).toBeGreaterThanOrEqual(0);
  });

  it('draws the compaction point where the boundary landed', () => {
    const boundary = {
      type: 'system',
      subtype: 'compact_boundary',
      uuid: 'cb-1',
      compact_metadata: { trigger: 'auto', pre_tokens: 68_031 },
    };

    const body = draw(
      prompt('compact it and carry on'),
      said([{ type: 'text', text: 'Folding the earlier context down first.' }]),
      boundary,
    );

    expect(body, 'the boundary leaves the row the settled shape draws').toContain('class="cpoint"');
    expect(body, 'carrying the count the frame gave it').toContain('68.0k before');
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

  it('tags the org by the seat the page is drawing, not by the row alone', () => {
    // Whether the tag appears is a comparison against THIS reader's org, so
    // the row cannot decide it: the seat has to reach the fold. A turn drawn
    // with no seat in hand claims no tag rather than guessing at a stranger.
    const envelope = (org: string): unknown => ({
      type: 'user',
      message: {
        role: 'user',
        content: [
          {
            type: 'text',
            text: `[Message id=m-1 from agent 'gateway-backend' (org '${org}')]\n\nhi`,
          },
        ],
      },
      uuid: 'u1',
    });
    const seat = { org: 'Busytools', project: 'forge', label: 'chat-kinds' };
    const drawSeated = (org: string): string =>
      render(Turn, {
        props: {
          turn: { key: 't1', messages: [envelope(org)], live: false } as HeldTurn,
          slot: seat,
        },
      }).body;

    expect(drawSeated('Gateway'), 'a counterparty outside the reader own org is tagged').toContain(
      '<span class="org">Gateway</span>',
    );
    expect(drawSeated('Busytools'), 'and one inside it carries no tag at all').not.toContain(
      'class="org"',
    );
    expect(
      draw(envelope('Gateway')),
      'and with no seat to compare against, the org the envelope carries stands as it is',
    ).toContain('<span class="org">Gateway</span>');
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
