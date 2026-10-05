<script lang="ts">
  /**
   * The composer, drawn in every state it takes, so the page can be put beside
   * `docs/book/src/ui/client/web-composer.html` without a running forge.
   *
   * The mockup's own order, and the mockup's own words: this is the page the
   * by-width check reads, and a specimen whose copy drifts from the drawing
   * makes the comparison say something the drawing does not.
   *
   * Every state the component can be in is here, including the three that no
   * prop reaches: the in-flight slash command, which comes from text the reader
   * sent (`Driven` types it), and the two one-shot states a take leaves behind.
   *
   * Reached only from the development route, and only through `fixture.ts`'s
   * deferred import, so neither it nor its specimens reach the shipped bundle.
   */
  import Composer from '../composer/Composer.svelte';
  import Queue from '../chat/Queue.svelte';
  import Driven from './Driven.svelte';
  import type {
    ComposerConnection,
    ComposerProps,
    ComposerRecord,
    SeatRead,
  } from '../composer/view';
  // The Queue specimen's own prop is the socket-shaped one, and the stand-in
  // above is the composer's narrower view of it.
  import type { Connection } from '../socket';
  import type { SessionSlot } from '../wire/types';

  const SLOT: SessionSlot = { org: 'Busytools', project: 'forge', label: 'lead' };

  /**
   * A connection that sends nothing and records everything.
   *
   * The page is drawn rather than driven, so there is no core to answer - but
   * what a dock ANSWERS with is the thing a by-width check cannot see by
   * looking, so every command lands on `window.__composerSent` where a check
   * can read it back.
   */
  const sent: unknown[] = [];
  const idle = {
    dispatch: (command: unknown) => {
      sent.push(command);
      (globalThis as { __composerSent?: unknown[] }).__composerSent = sent;
      return null;
    },
    devices: () => true,
    onMessage: () => () => {},
    store: () => undefined,
  } as unknown as ComposerConnection;

  function blank(): ComposerRecord {
    return {
      slot: { org: 'Busytools', project: 'forge', label: 'lead' },
      composer: { take: null, notice: null, compacting: false, sign_in: null },
      pending_asks: [],
      header: { turn_in_flight: false },
      slash_commands: [
        { name: '/clear', description: 'Clear chat history' },
        { name: '/help', description: 'Show help' },
      ],
      subagents: [
        { name: 'cli-version', description: 'settled 3m' },
        { name: 'cli-audit', description: 'never run' },
      ],
      file_index: { entries: {} },
    };
  }

  function seat(over: Partial<SeatRead> = {}): SeatRead {
    return { lifecycle: 'Running', reason: null, waking: false, pendingDepth: 1, ...over };
  }

  /** A specimen's props with the seat, record and connection every composer needs. */
  function withDefaults(over: Partial<ComposerProps>): ComposerProps {
    return {
      record: over.record ?? blank(),
      slot: SLOT,
      seat: over.seat ?? seat(),
      connection: idle,
      dictation: over.dictation ?? false,
    };
  }

  /** A take as the wire carries one, which is what a record holds. */
  function take(over: Record<string, unknown> = {}): Record<string, unknown> {
    return {
      phase: 'recording',
      // The mockup's own envelope, so the meter reads as the drawing's does.
      levels: [
        0.12, 0.26, 0.48, 0.72, 0.94, 0.64, 0.4, 0.26, 0.14, 0.1, 0.3, 0.54, 0.78, 0.96, 0.7, 0.46,
        0.28, 0.14, 0.1, 0.22, 0.44, 0.66, 0.5, 0.34, 0.2, 0.12,
      ],
      peak_db: -18,
      progress: [0, null],
      floor_db: -50,
      elapsed_ms: 7000,
      ...over,
    };
  }

  const permission = {
    kind: 'permission',
    request: {
      tool_call: {
        tool_call_id: 'tu-1',
        title: 'Bash',
        kind: 'execute',
        status: 'pending',
        content: [],
        locations: [],
        raw_input: { command: 'git push origin polish/rate-limit-chip-softer-34' },
      },
      display: { title: 'Bash', display_name: null, description: null, decision_reason: null },
      options: [
        { option_id: 'once', name: 'Allow once', kind: 'allow', action: { kind: 'allow' } },
        {
          option_id: 'always',
          name: 'Allow always for Bash · git push *',
          kind: 'allow',
          action: { kind: 'allow' },
        },
        {
          option_id: 'edit',
          name: 'Allow with edits',
          kind: 'edit',
          action: { kind: 'allow_with_input' },
        },
        { option_id: 'deny', name: 'Deny', kind: 'deny', action: { kind: 'deny' } },
        {
          option_id: 'notes',
          name: 'Tell the agent something else',
          kind: 'notes',
          action: { kind: 'deny' },
        },
      ],
    },
  };

  const readOutside = {
    kind: 'permission',
    request: {
      ...permission.request,
      tool_call: {
        ...permission.request.tool_call,
        tool_call_id: 'tu-2',
        title: 'Read',
        raw_input: { file_path: '/tmp/forge-deny-scenario.txt' },
      },
      display: {
        title: 'Read',
        display_name: null,
        decision_reason: 'Path is outside allowed working directories',
        description: 'Reads the contents of a file outside this project.',
      },
      options: permission.request.options.slice(0, 1).concat(permission.request.options.slice(3)),
    },
  };

  const slackDraft = {
    kind: 'slack_draft',
    request: {
      id: 'draft-1',
      workspace: 'Trust Machines',
      conversation_label: '#granite-staging-alerts',
      thread_ts: null,
      text: 'The staging deploy is green. Rolling to production once the 14:30 window closes.',
    },
  };

  const slackReply = {
    kind: 'slack_draft',
    request: {
      ...slackDraft.request,
      id: 'draft-2',
      thread_ts: '1790956000.969469',
      text: 'Confirmed - the replica caught the bad statement, so production is clear to take the same set.',
    },
  };

  const questionSingle = {
    kind: 'question',
    request: {
      tool_call: {
        tool_call_id: 'tu-q1',
        title: 'AskUserQuestion',
        kind: 'other',
        status: 'pending',
        content: [],
        locations: [],
        raw_input: {},
      },
      prompt: {
        header: 'Environments',
        question: 'Pick the environment to deploy to.',
        multi_select: false,
        options: [
          {
            option_id: 'staging',
            label: 'Staging',
            description: 'the pre-production cluster',
            preview: '**staging** · deploys run the full migration set against the replica first.',
          },
          {
            option_id: 'prod',
            label: 'Production',
            description:
              'live traffic, and the migration set runs against the replica first so a bad statement is caught before it reaches a customer',
            preview: 'Rolling deploy · **five minutes of 1% traffic** before the rest.',
          },
          {
            option_id: 'dev',
            label: 'Development',
            description: null,
            preview: '```sh\ndeploy --env dev --skip-migrations\n```',
          },
        ],
      },
      question_index: 1,
      total_questions: 3,
    },
  };

  const question = {
    kind: 'question',
    request: {
      tool_call: {
        tool_call_id: 'tu-q',
        title: 'AskUserQuestion',
        kind: 'other',
        status: 'pending',
        content: [],
        locations: [],
        raw_input: {},
      },
      prompt: {
        header: 'Environments',
        question: 'Pick the environments to deploy to.',
        multi_select: true,
        options: [
          {
            option_id: 'staging',
            label: 'Staging',
            description: 'the pre-production cluster',
            preview: '**staging** · deploys run the full migration set against the replica first.',
          },
          {
            option_id: 'prod',
            label: 'Production',
            description:
              'live traffic, and the migration set runs against the replica first so a bad statement is caught before it reaches a customer',
            preview: 'Rolling deploy · **five minutes of 1% traffic** before the rest.',
          },
          {
            option_id: 'dev',
            label: 'Development',
            description: null,
            preview: '```sh\ndeploy --env dev --skip-migrations\n```',
          },
        ],
      },
      question_index: 1,
      total_questions: 3,
    },
  };

  /**
   * One specimen: what it is called, the composer it draws, and - for a state
   * only the reader can produce - the text to type into it.
   */
  const specimens: { label: string; props: Partial<ComposerProps>; driven?: string }[] = [
    { label: 'idle', props: {} },
    {
      label: 'hint · authentication',
      props: {
        seat: seat({ lifecycle: 'AuthRequired' }),
        record: {
          ...blank(),
          composer: {
            take: null,
            notice: null,
            compacting: false,
            sign_in: {
              method_name: 'claude.ai · Anthropic OAuth (Pro)',
              method_description: '',
            },
          },
        },
      },
    },
    {
      label: 'recording',
      props: {
        dictation: true,
        record: {
          ...blank(),
          composer: { take: take(), notice: null, compacting: false, sign_in: null },
        },
      },
    },
    {
      label: 'transcribing',
      props: {
        dictation: true,
        record: {
          ...blank(),
          composer: {
            take: take({ phase: 'transcribing', progress: [2, 6], peak_db: -22 }),
            notice: null,
            compacting: false,
            sign_in: null,
          },
        },
      },
    },
    {
      label: 'notice · quiet room',
      props: {
        dictation: true,
        record: {
          ...blank(),
          composer: {
            take: null,
            notice: {
              kind: 'line',
              tone: 'q',
              text: 'nothing above -50 dBFS in 4s · loudest was -38.2 · try again',
            },
            compacting: false,
            sign_in: null,
          },
        },
      },
    },
    {
      label: 'notice · what the core refused',
      props: {
        dictation: true,
        record: {
          ...blank(),
          composer: {
            take: null,
            notice: {
              kind: 'line',
              tone: 'bad',
              text: 'microphone is held by another application',
            },
            compacting: false,
            sign_in: null,
          },
        },
      },
    },
    {
      label: 'landed · one green beat, then idle',
      props: {
        record: {
          ...blank(),
          composer: {
            take: null,
            notice: { kind: 'landed', text: 'fix the flaky retry test and', truncated: false },
            compacting: false,
            sign_in: null,
          },
        },
      },
    },
    {
      label: 'blocking · connecting',
      props: { seat: seat({ lifecycle: 'Spawning' }) },
    },
    {
      label: 'blocking · a slash command in flight (typed, then sent)',
      props: { record: { ...blank(), header: { turn_in_flight: true } } },
      driven: '/compact',
    },
    {
      label: 'blocking · compacting',
      props: {
        record: {
          ...blank(),
          composer: { take: null, notice: null, compacting: true, sign_in: null },
        },
      },
    },
    {
      label: 'blocking · error',
      props: {
        seat: seat({ lifecycle: 'Failed', reason: 'the CLI exited with status 1' }),
      },
    },
    {
      label: 'blocking · not running',
      props: {
        seat: seat({ waking: true, reason: 'this seat has no session behind it' }),
      },
    },
    { label: 'dock · permission', props: { record: { ...blank(), pending_asks: [permission] } } },
    {
      label: 'dock · with a take still running behind it',
      props: {
        record: {
          ...blank(),
          pending_asks: [permission],
          composer: { take: take(), notice: null, compacting: false, sign_in: null },
        },
      },
    },
    {
      label: "dock · with the CLI's own reason",
      props: { record: { ...blank(), pending_asks: [readOutside] } },
    },
    { label: 'dock · slack draft', props: { record: { ...blank(), pending_asks: [slackDraft] } } },
    { label: 'dock · slack reply', props: { record: { ...blank(), pending_asks: [slackReply] } } },
    {
      label: 'dock · question · one answer',
      props: { record: { ...blank(), pending_asks: [questionSingle] } },
    },
    {
      label: 'dock · question · several answers',
      props: { record: { ...blank(), pending_asks: [question] } },
    },
    {
      label: 'dock · queued behind another',
      props: {
        seat: seat({ pendingDepth: 3 }),
        record: { ...blank(), pending_asks: [permission] },
      },
    },
  ];

  /**
   * The prompts waiting in the CLI's queue, in the pile's own specimen.
   *
   * **The words fill their line or the card is broken**: this is what a
   * real-engine measurement and the eye read for #1705, and the mockup's own
   * copy is kept so the two can be set side by side.
   */
  const pile = [
    { uuid: 'q1', source: 'you', text: 'fix the flaky retry test before the gate' },
    { uuid: 'q2', source: 'cron', text: 'nightly sweep: re-run the bench suite' },
  ];
</script>

<div class="states">
  {#each specimens as specimen (specimen.label)}
    <section>
      <h3>{specimen.label}</h3>
      {#if specimen.driven !== undefined}
        <Driven command={specimen.driven} props={withDefaults(specimen.props)} />
      {:else}
        <div class="comp">
          <Composer {...withDefaults(specimen.props)} />
        </div>
      {/if}
    </section>
  {/each}
  <section>
    <h3>queue · two waiting, the newest in front</h3>
    <div class="comp">
      <div class="composer">
        <Queue rows={pile} ended={null} slot={SLOT} connection={idle as unknown as Connection} />
      </div>
    </div>
  </section>
</div>

<style>
  /* The specimen page's own framing, which is not the client's theme: the
     composer's slot in a real page is the session column's foot, and what is
     being looked at here is the box, not the page around it. */
  .states {
    display: grid;
    /* `min()` so a phone gets one column the width of the screen rather than a
       420px cell the page scrolls sideways to reach: the composer is the thing
       being measured at that width, not this grid. */
    grid-template-columns: repeat(auto-fill, minmax(min(100%, 420px), 1fr));
    gap: 22px;
    padding: 26px 30px 60px;
  }

  h3 {
    color: var(--dim);
    font-family: var(--mono);
    font-size: var(--fs-label);
    letter-spacing: 0.06em;
    margin-bottom: 6px;
    text-transform: uppercase;
  }

  section {
    min-width: 0;
  }
</style>
