import { render } from 'svelte/server';
import { afterEach, describe, expect, it } from 'vitest';

import McpSegment from './McpSegment.svelte';
import { mcp } from './mcp.svelte';

afterEach(() => {
  mcp.sync(null);
});

const SERVERS = [
  {
    name: 'forge',
    k: 'forge \u{b7} session',
    v: '2 tools',
    tools: [{ name: 'roster', description: null }],
    command: 'node /opt/mcp-servers/forge-server.js',
    reason: null,
  },
  {
    name: 'vercel',
    k: 'vercel \u{b7} session',
    v: 'needs sign-in',
    tools: [],
    command: null,
    reason: 'OAuth token expired',
  },
];

describe('the MCP segment', () => {
  it('carries the MCP glyph, the count and the wording', () => {
    mcp.sync(SERVERS);
    const body = render(McpSegment, {}).body;

    expect(body, 'the glyph says what the segment is').toContain('i-mcp');
    expect(body, 'the plural count reads plainly').toContain('2 servers');
  });

  it('counts a single server in the singular', () => {
    mcp.sync(SERVERS.slice(0, 1));
    const body = render(McpSegment, {}).body;

    expect(body, 'one reads as one').toContain('1 server');
    expect(body, 'and not as a plural').not.toContain('1 servers');
  });

  it('draws nothing for a session that reported no servers', () => {
    mcp.sync(null);
    const body = render(McpSegment, {}).body;

    expect(body, 'no segment at all').not.toContain('sg-seg');
  });
});
