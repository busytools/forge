/**
 * The floor: the shapes a protocol-4 server sends, folded through this
 * client's own read path.
 *
 * A skew is tolerated because the read is proven rather than because it is
 * believed harmless, and this file is the proof. It is what keeps
 * `MIN_PROTOCOL` honest: if a later bump breaks the read of the floor's
 * shapes, that shows up here rather than in a person's session.
 *
 * **Where the fixture came from.** `fixtures/protocol-4.json` is derived from
 * the archived record under
 * `crates/forge-test-harness/baselines/socket/4/` - the frame census and the
 * greeting's field set are what a v4 server emitted, and the greeting carries
 * no release fields because v4 predates them - and from the v4-era snapshot
 * fixtures, which is where the composer's `take` and `notice` come from. That
 * pair is the WHOLE wire difference between 4 and 5: a take belongs to the
 * connection that started it, and no other reader may draw it.
 *
 * **What this cannot see.** The fixture carries what these folds read, not
 * every field a v4 server could send. A field this client reads that the
 * fixture happens not to carry is a read this file does not prove.
 */

import { describe, expect, it } from 'vitest';

import { composerFrom } from '../composer/wire';
import { MIN_PROTOCOL, skewOf, type ServerMessage } from '../protocol';
import { applyUpdate } from '../session/apply';
import { sessionFrom } from '../session/wire';
import fixture from './fixtures/protocol-4.json';
import { homeFrom, type HomeWire } from './home';
import { settingsFrom } from './types';

/**
 * The greeting the fixture carries, narrowed as the socket narrows it.
 *
 * The cast is the JSON widening: TypeScript reads a JSON string as `string`,
 * and the message union needs the literal tag.
 */
const greeting = fixture.greeting as unknown as Extract<ServerMessage, { kind: 'greeting' }>;

describe('the greeting the floor speaks', () => {
  /**
   * The fixture IS the floor. A bump that moves `MIN_PROTOCOL` without
   * replacing this fixture would leave the floor standing on shapes nothing
   * checks, which is the one failure this file exists to prevent.
   */
  it('is the version the floor names', () => {
    expect(greeting.version).toBe(MIN_PROTOCOL);
    // A v4 server predates the greeting's release fields, which is why the
    // common skew has no build to name for the server half.
    expect(greeting).not.toHaveProperty('forge_version');
    expect(greeting).not.toHaveProperty('forge_version_short');
  });

  it('reads as a skew with no build named', () => {
    expect(skewOf(greeting)).toEqual({ serverProtocol: MIN_PROTOCOL, serverVersion: null });
  });

  it('carries the client settings, axes included', () => {
    expect(settingsFrom(greeting.settings)).toEqual({
      mark: 'klin',
      theme: 'night',
      font: 'system',
      dictate: { styling: 'casual', structure: 'lists', context: 'email' },
    });
  });
});

describe('the home the floor sends', () => {
  it('reads every field the home fold narrows', () => {
    // The cast is why a JSON import cannot be typed as `HomeWire`: TypeScript
    // widens a JSON string literal to `string`, which no union accepts - and
    // the fold below is what narrows it. The dev fixtures cast the same way.
    const home = homeFrom(fixture.home as unknown as HomeWire);
    // Each of these is a member `homeFrom` narrows or counts, so a value
    // arriving as its fallback is a read that stopped working.
    expect(home.projects[0]?.work.gate).toBe('in_repo');
    expect(home.projects[0]?.tasks[0]?.status).toBe('in_progress');
    expect(home.agents[0]?.lifecycle).toBe('Running');
    expect(home.agents[0]?.pending).toBe('question');
    expect(home.agents[0]?.work?.gate).toBe('in_repo');
    // A floor server names no failure at all, and an absent field must read
    // as `null` rather than as a failure: `undefined !== null` is the one
    // that made every row draw the cross when the fallback was missing.
    expect(home.agents[0]?.failed_turn, 'a floor server names no failure').toBeNull();
    expect(home.accounts.loading[0]?.state).toBe('ready');
    expect(home.dictate.snapshot.models[0]?.state).toBe('ready');
    expect(home.forge_version_short, 'the build the home names').toBe('1.0.112+abc1234');
    expect(home.cli_version?.installed).toBe('2.0.0');
  });
});

describe('the session the floor sends', () => {
  const held = sessionFrom(fixture.session);

  it('reads the header whole', () => {
    expect(held.header).toEqual({
      session_id: '11111111-2222-3333-4444-555555555555',
      model: { resolved_id: 'claude-opus-5', display_name_long: 'Opus 5' },
      effort: 'high',
      permission_mode: 'plan',
      context: { percent: 42, max_tokens: 200000 },
      available_models: [{ resolved_id: 'claude-opus-5' }],
      turn_in_flight: true,
    });
  });

  it('reads the sets and the seat state whole', () => {
    expect(held.state.scan_cwd).toBe('/tmp/protocol-4/proj');
    expect(held.queue).toEqual([{ uuid: 'q1', source: 'cron', text: 'wake up' }]);
    expect(held.mcp?.servers[0]?.name).toBe('forge');
    expect(held.processes?.processes[0]?.pid).toBe(4242);
    expect(held.monitors[0]?.command).toBe('gh run watch 1');
    expect(held.background_tasks).toHaveLength(1);
    expect(held.slash_commands).toEqual([{ name: 'review', description: 'review it' }]);
    expect(held.subagents).toEqual([{ name: 'general-purpose', description: 'does things' }]);
    expect(held.subagent_instances[0]).toMatchObject({
      dispatch_id: 'tu-sub',
      calls: 1,
      tail: [{ name: 'Read', title: 'Read src/lib.rs', status: 'completed' }],
    });
    expect(held.pending_asks).toHaveLength(1);
  });

  it('reads the work, the pull request and the issues it closes', () => {
    expect(held.work).toEqual({ branch: 'protocol-4', changed: 3, gate: 'in_repo' });
    expect(held.pr).toEqual({
      number: 1234,
      url: 'https://example.test/pull/1234',
      draft: false,
    });
    expect(held.closes).toEqual([{ number: 1200, url: 'https://example.test/issues/1200' }]);
  });

  it('reads the conversation it was handed', () => {
    expect(held.conversation.compaction_count).toBe(2);
    expect(held.conversation.turns).toHaveLength(1);
    expect(held.has_dispatches).toBe(true);
  });

  /**
   * The v4 record carries a take and a notice, and the v5 read drops both:
   * a take belongs to the connection that started it, so they are this
   * client's own state, built from the take's own updates. This is the one
   * deliberate loss in the whole fixture, and the reason it is safe to read
   * a v4 server is that NOTHING else is lost.
   */
  it('drops the take and the notice a v4 record carried, and reads the rest', () => {
    expect(held.composer).toEqual({
      take: null,
      notice: null,
      compacting: true,
      sign_in: { method_name: 'first_party', method_description: 'Sign in with Claude' },
    });
  });

  it('hands the composer fold a record it reads', () => {
    const composer = composerFrom(held.composer);
    expect(composer.compacting).toBe(true);
    expect(composer.signIn).toEqual({
      methodName: 'first_party',
      methodDescription: 'Sign in with Claude',
    });
    expect(composer.take, 'a take is the live one, never the snapshot').toBeNull();
  });
});

describe('the updates the floor sends', () => {
  /** The record as a page holds it, with every fixture frame folded in. */
  const folded = fixture.updates.reduce(
    (held, frame) => applyUpdate(held, frame.update),
    sessionFrom(fixture.session),
  );

  it('folds the header, work and conversation frames', () => {
    expect(folded.header.context).toEqual({ percent: 73, max_tokens: 200000 });
    expect(folded.work).toEqual({ branch: 'protocol-4', changed: 9, gate: 'in_repo' });
    expect(folded.pr).toBeNull();
    expect(folded.closes).toEqual([]);
    // The frame's own words survive the chat fold the record wraps.
    expect(JSON.stringify(folded.conversation)).toContain('still reading the floor');
  });

  it('folds the take the floor streams, from its own updates', () => {
    const take = folded.composer.take as Record<string, unknown> | null;
    expect(take).not.toBeNull();
    expect(take?.['phase']).toBe('recording');
    expect(take?.['floor_db']).toBe(-50);
    // The reading crosses as the peak over the take's own range: -20 dBFS
    // between the -50 the frame declared and the meter's 0 ceiling.
    expect(take?.['levels']).toEqual([0.6]);
  });

  it('settles the turn the unit frame names', () => {
    expect(folded.header.turn_in_flight).toBe(false);
  });
});
