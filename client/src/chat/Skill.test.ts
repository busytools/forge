import { render } from 'svelte/server';
import { describe, expect, it } from 'vitest';

import Skill from './Skill.svelte';

/**
 * The skill row, as markup.
 *
 * **What this can answer and what it cannot.** The row's whole job is to say
 * which skill loaded and to hold its body readable, so the thing to assert is
 * text: the name on the summary and the skill's markdown rendered whole behind
 * it. Type and colour are the sheet's, which the by-width measurement covers.
 */

const BODY =
  '# Unslop\n\nEdit text to remove AI patterns, **especially** the tells.\n\n- one\n- two\n\n`cargo check`';

const body = render(Skill, { props: { name: 'unslop', body: BODY } }).body;

describe('a skill the CLI loaded', () => {
  it('names the skill on its own row, with the shared chevron', () => {
    expect(body, 'the row says which skill loaded').toContain('skill /unslop');
    expect(body, 'and carries the disclosure chevron').toContain('#i-chev');
    expect(body, 'and its own mark').toContain('#i-skill');
  });

  it('holds the whole body as markdown, not as raw text', () => {
    // The body arrives as the reader's own user frame and this row is what
    // takes it off their attribution; what it must not lose on the way is the
    // skill's own formatting.
    expect(body, 'the heading is a heading').toContain('<h1>');
    expect(body, 'the list is a list').toContain('<li>');
    expect(body, 'bold draws bold').toContain('<strong>especially</strong>');
    expect(body, 'and code keeps its own face').toContain('<code>');
    expect(body, 'with none of the marks shown raw').not.toContain('**');
  });
});
