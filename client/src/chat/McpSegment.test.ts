import { render } from 'svelte/server';
import { afterEach, describe, expect, it } from 'vitest';

import McpSegment from './McpSegment.svelte';
import { mcp } from './mcp.svelte';
import type { McpRow } from '../session/view';

afterEach(() => {
  mcp.sync(null);
});

const SERVERS: McpRow[] = [
  {
    name: 'forge',
    k: 'forge \u{b7} session',
    v: '2 tools',
    tools: ['roster', 'session'],
    command: 'node /opt/mcp-servers/forge-server.js',
    reason: null,
    synthetic: false,
  },
  {
    name: 'vercel',
    k: 'vercel \u{b7} session',
    v: 'needs sign-in',
    tools: [],
    command: null,
    reason: 'OAuth token expired',
    synthetic: false,
  },
];

describe('the MCP segment', () => {
  it('carries the MCP glyph, the count and the wording', () => {
    mcp.sync(SERVERS);
    const body = render(McpSegment, {}).body;

    expect(body, 'the glyph says what the segment is').toContain('i-mcp');
    expect(body, 'the plural count reads plainly').toContain('2 MCPs');
  });

  it('counts a single server in the singular', () => {
    mcp.sync(SERVERS.slice(0, 1));
    const body = render(McpSegment, {}).body;

    expect(body, 'one reads as one').toContain('1 MCP');
    expect(body, 'and not as a plural').not.toContain('1 MCPs');
  });

  /**
   * A read that failed draws as its own row, and that row is not a server:
   * the toggle must state the failure rather than count it.
   */
  it('states a failed read rather than counting its row as a server', () => {
    mcp.sync([
      {
        name: 'mcp-read',
        k: 'servers',
        v: 'failed',
        tools: [],
        command: null,
        reason: 'the CLI refused',
        synthetic: true,
      },
    ]);
    const body = render(McpSegment, {}).body;

    expect(body, 'the failure is stated').toContain('MCP failed');
    expect(body, 'and not miscounted as a server').not.toContain('1 MCP');
  });

  it('draws nothing for a session that reported no servers', () => {
    mcp.sync(null);
    const body = render(McpSegment, {}).body;

    expect(body, 'no segment at all').not.toContain('sg-seg');
  });
});
