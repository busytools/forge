/**
 * The two turns #2503's transcript holds, by their own frames.
 *
 * Space Acres session `b837ca51`, resumed 2026-10-22: one turn ran on Oct 18 and
 * the file holds no `result` frame anywhere, so a turn read off it settles only
 * by a newer turn following it. The client held the Oct 18 turn as the live one
 * and, when the resume's page landed, still pinned it - its clock counting the
 * four days of downtime.
 *
 * The frames are the transcript's own rows at the turn boundaries, cut to what
 * the fold reads: the ids and stamps are the file's, so the two slices share no
 * frame and nothing reconciles them.
 */

/** The turn as the frames built it before the resume: no result, so still live. */
export const PRE_RESUME: unknown[] = [
  {
    type: 'user',
    uuid: 'forge_prompt_206_c6a7205c',
    timestamp: '2026-10-18T04:36:37.303Z',
    message: {
      role: 'user',
      content: [
        {
          type: 'text',
          text: "[Message id=m-58238410 from agent 'dotfiles' (org 'Personal')]\n\nWeekly audit ask",
        },
      ],
    },
  },
  {
    type: 'assistant',
    uuid: '52b5bdf1-5de8-44e9-ac64-f1b18008070d',
    timestamp: '2026-10-18T04:36:42.503Z',
    message: {
      id: 'msg_oct_1',
      role: 'assistant',
      model: 'claude-opus-5-5',
      content: [{ type: 'thinking', thinking: 'reading the ask' }],
      usage: { input_tokens: 4, output_tokens: 20 },
    },
  },
  {
    type: 'assistant',
    uuid: '0830f15d-8c41-4b2e-b090-e052b51519a3',
    timestamp: '2026-10-18T04:36:42.807Z',
    message: {
      id: 'msg_oct_1',
      role: 'assistant',
      model: 'claude-opus-5-5',
      content: [{ type: 'tool_use', id: 'toolu_oct_1', name: 'Bash', input: { command: 'date' } }],
      usage: { input_tokens: 4, output_tokens: 20 },
    },
  },
  {
    type: 'user',
    uuid: '47398c0d-0264-4e6b-9a4d-c3cb4efad3ba',
    timestamp: '2026-10-18T06:07:51.812Z',
    message: {
      role: 'user',
      content: [{ type: 'tool_result', tool_use_id: 'toolu_oct_1', content: 'ok' }],
    },
  },
  {
    type: 'assistant',
    uuid: '4bcfadbf-fb55-4df2-88f2-14fadd6851dc',
    timestamp: '2026-10-18T06:07:55.644Z',
    message: {
      id: 'msg_oct_2',
      role: 'assistant',
      model: 'claude-opus-5-5',
      content: [{ type: 'text', text: 'The crash fix stays on hold.' }],
      usage: { input_tokens: 4, output_tokens: 20 },
    },
  },
  {
    type: 'system',
    subtype: 'stop_hook_summary',
    uuid: 'b3bb6d20-3c1a-4d0e-9d1b-4a1a4c6f2e44',
    timestamp: '2026-10-18T06:07:55.741Z',
  },
];

/** What the server names the resumed turn by: the mid-turn prompt that opened it. */
export const RESUME_KEY = '2026-10-22T10:20:25Z';

/** The instant the resumed turn's own rows end, which is where its clock stops. */
export const RESUME_END = '2026-10-22T10:29:17.530Z';

/** The turn the resume page serves: one turn, keyed by its prompt's clock. */
export const RESUME: unknown[] = [
  {
    type: 'user',
    uuid: 'bd1dd4c5-9a88-458a-846b-01162cac2f10',
    timestamp: '2026-10-22T10:20:25.181Z',
    message: {
      role: 'user',
      content: [
        {
          type: 'queued_command',
          prompt: 'Get up to speed, do what is requested, and then wait for me before you push up.',
          source_uuid: 'a0009386-bfba-42fa-82f5-6d1157461e2c',
          commandMode: 'prompt',
        },
      ],
    },
  },
  {
    type: 'assistant',
    uuid: 'f339ada5-450a-43b8-9983-b093f2fe2ace',
    timestamp: '2026-10-22T10:20:26.753Z',
    message: {
      id: 'msg_resume_1',
      role: 'assistant',
      model: 'claude-opus-5-5',
      content: [{ type: 'thinking', thinking: 'picking the ask up' }],
      usage: { input_tokens: 6, output_tokens: 30 },
    },
  },
  {
    type: 'assistant',
    uuid: '8b8b2e94-a550-4e5b-8676-2bb8035d6dcd',
    timestamp: '2026-10-22T10:20:30.526Z',
    message: {
      id: 'msg_resume_1',
      role: 'assistant',
      model: 'claude-opus-5-5',
      content: [
        { type: 'tool_use', id: 'toolu_resume_1', name: 'Bash', input: { command: 'git log' } },
      ],
      usage: { input_tokens: 6, output_tokens: 30 },
    },
  },
  {
    type: 'user',
    uuid: '81d22d86-5848-4e04-ada9-877c8c839b8b',
    timestamp: '2026-10-22T10:28:51.206Z',
    message: {
      role: 'user',
      content: [{ type: 'tool_result', tool_use_id: 'toolu_resume_1', content: 'ok' }],
    },
  },
  {
    type: 'assistant',
    uuid: '52b72997-09fb-4969-9104-a20bc05db6b5',
    timestamp: RESUME_END,
    message: {
      id: 'msg_resume_2',
      role: 'assistant',
      model: 'claude-opus-5-5',
      content: [{ type: 'text', text: 'Up to speed on the release ask.' }],
      usage: { input_tokens: 6, output_tokens: 30 },
    },
  },
];
