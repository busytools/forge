import { render } from 'svelte/server';
import { afterEach, describe, expect, it } from 'vitest';

import ConnectorsSegment from './ConnectorsSegment.svelte';
import { connectors } from './connectors.svelte';

afterEach(() => {
  connectors.sync(null);
});

describe('the connectors segment', () => {
  it('carries both connector marks, the count and the wording', () => {
    connectors.sync([
      { kind: 'gotify', id: 'g-1', key: 'client-alerts', value: '>=5' },
      { kind: 'slack', id: 's-1', key: 'forge', value: 'mentions anywhere \u{b7} mentions only' },
    ]);
    const body = render(ConnectorsSegment, {}).body;

    // Both marks on the toggle: it is the pair's subject, and the bell alone
    // would draw identically to a gotify row (same sprite path).
    expect(body, 'the gotify mark leads the toggle').toContain('i-gotify');
    expect(body, 'and the slack mark pairs with it').toContain('i-slack');
    expect(body, 'the plural count reads plainly').toContain('2 subscriptions');
  });

  it('counts a single subscription in the singular', () => {
    connectors.sync([{ kind: 'gotify', id: 'g-1', key: 'client-alerts', value: '>=5' }]);
    const body = render(ConnectorsSegment, {}).body;

    expect(body, 'one reads as one').toContain('1 subscription');
    expect(body, 'and not as a plural').not.toContain('1 subscriptions');
  });

  it('draws nothing for a seat that holds none', () => {
    connectors.sync(null);
    const body = render(ConnectorsSegment, {}).body;

    expect(body, 'no segment at all').not.toContain('sg-seg');
  });
});
