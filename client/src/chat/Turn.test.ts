import { render } from 'svelte/server';
import { describe, expect, it } from 'vitest';

import type { Turn as HeldTurn } from './conversation';
import Turn from './Turn.svelte';

/** A turn holding `messages`, drawn as the page draws it. */
const draw = (...messages: unknown[]): string =>
  render(Turn, { props: { turn: { key: 't1', messages, live: false } as HeldTurn, cwd: null } })
    .body;

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
    const mine = between(body, '<div class="mine"', '</div>');
    expect(mine, 'the words are still the turn').toContain('what is wrong with this layout?');
    expect(mine, 'and what came with them is named').toContain('image/png');
    expect(mine, 'with the size the payload states').toContain('68 B');
    expect(body, 'and no bytes in the page').not.toContain('iVBORw0KGgo');
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
});
