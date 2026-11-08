/**
 * A `forge` MCP call's own card, read off the call's result.
 *
 * The server answers each family in a shape of its own, and this module turns
 * that JSON into the parts a row draws: what the row is titled with (the
 * subject the call acted on, never the tool's name), the words its chips
 * carry, the figure at its right, and what the body says. `null` is the
 * raw-text fallback every other call gets - a result this cannot read is
 * never dropped, only left undressed, and a failed call is null so its reason
 * draws the way every failure draws.
 */

import { untilOf } from '../session/view';
import { forgeFamilyOf, type ForgeFamily } from './families';
import { firstText, obj, parsedText, parsedTextPastClock, str, trailingTexts } from './result-json';
import { isSlackId } from './text';

/** One word on a forge row. The tone colours the word; the word carries it. */
export interface ForgeChip {
  text: string;
  tone: 'plain' | 'info' | 'ok' | 'warn' | 'bad' | 'dim';
}

/** One entry of a card's body list. */
interface ForgeListItem {
  id: string | null;
  state: ForgeChip | null;
  text: string;
  /** A dim mono marker beside the text: a cron's expression, a target's kind. */
  tag: string | null;
  when: string | null;
}

/**
 * One review comment as its own block: where it sits, the code it was filed
 * against, and what has been said about it.
 *
 * The context lines carry no numbers: the wire sends the captured lines as
 * strings, and a numbered gutter this page worked out for itself would be a
 * line number nobody measured.
 */
interface ForgeComment {
  /** `file:line`, the way the review tools name the spot. */
  where: string;
  /** The side of the diff it sits on, as the comment's own words. */
  side: string;
  state: ForgeChip;
  context: string[];
  turns: { author: string; text: string; you: boolean }[];
}

/** What a card's body draws, in the row grammar the mock settled. */
export type ForgePiece =
  | { kind: 'kv'; pairs: [string, string][] }
  | { kind: 'quote'; text: string }
  | { kind: 'tag'; text: string }
  | { kind: 'list'; items: ForgeListItem[] }
  | { kind: 'warnline'; label: string; text: string }
  | { kind: 'comments'; items: ForgeComment[] }
  /** What a query found nothing of, said in words rather than drawn as a gap. */
  | { kind: 'empty'; text: string }
  /**
   * A block the card itself does not read - a result's second text part -
   * drawn as the words it is rather than dropped.
   */
  | { kind: 'text'; text: string };

/** One forge call as its card. */
export interface ForgeCard {
  /** The row's title: the subject, never the tool's name. */
  title: string;
  chips: ForgeChip[];
  figure: string | null;
  pieces: ForgePiece[];
  /**
   * A proportion the row draws as a meter, for a card whose subject IS a
   * count against a limit: the capacity row.
   */
  meter?: { fill: number; of: number } | null;
  /**
   * A line after the title that is a result rather than a failure, with the
   * word it is read by: a despawn that stopped at a dirty worktree says so on
   * the row, where a failed CALL says its reason there too.
   */
  tail?: { text: string; tone: 'warn' } | null;
  /**
   * The mark this row draws instead of its family's, for a card whose subject
   * is not the family's: a capacity reads as a gauge, not as the worker glyph
   * every other agents row carries.
   */
  glyph?: string | null;
}

/**
 * The gotify family's card: a subscription's filter IS the card.
 *
 * `subscribe` answers one structured row, `unsubscribe` the removed row, and
 * `list` / `apps` / `recent` their arrays - all read here so a reader sees
 * what is watched rather than a uuid.
 */
function gotifyCard(verb: string, answer: unknown): ForgeCard | null {
  if (verb === 'subscribe') {
    const sub = subOf(answer);
    if (sub === null) return null;
    const pieces: ForgePiece[] = [
      {
        kind: 'kv',
        pairs: [
          ['subscription', shortId(sub.id)],
          ['delivers', 'as a turn, waking this session'],
        ],
      },
    ];
    if (!sub.names_resolve) {
      pieces.unshift({
        kind: 'warnline',
        label: 'warning',
        text: 'the application index could not be refreshed, so filters naming apps will not match until the stream reconnects',
      });
    }
    return {
      title: 'watching notifications',
      chips: subscriptionChips(sub),
      figure: sub.names_resolve ? null : 'index stale',
      pieces,
    };
  }
  if (verb === 'unsubscribe') {
    const envelope = obj(answer);
    if (str(envelope, 'status') !== 'deleted') return null;
    const sub = subOf(envelope['removed']);
    if (sub === null) return null;
    return {
      title: 'stopped watching',
      // The filter that stopped, as ONE chip: the row reads as the record of
      // a thing that is over, not as a live filter with parts.
      chips: [
        {
          text: `${appsText(sub.applications)} \u{b7} ${floorText(sub.min_priority)}`,
          tone: 'dim',
        },
      ],
      figure: null,
      pieces: [],
    };
  }
  if (verb === 'list') {
    if (!Array.isArray(answer)) return null;
    const items: ForgeListItem[] = [];
    for (const held of answer) {
      const sub = subOf(held);
      if (sub === null) return null;
      items.push({
        id: null,
        state: null,
        text: appsText(sub.applications),
        tag: floorText(sub.min_priority),
        when: shortId(sub.id),
      });
    }
    return {
      title: 'subscriptions',
      chips: [],
      figure: items.length === 0 ? null : `${items.length} active`,
      pieces:
        items.length === 0
          ? [
              {
                kind: 'empty',
                text: 'no Gotify subscriptions - nothing from the server reaches this session',
              },
            ]
          : [{ kind: 'list', items }],
    };
  }
  if (verb === 'apps') {
    if (!Array.isArray(answer)) return null;
    const names = answer.filter((held): held is string => typeof held === 'string');
    const items: ForgeListItem[] = names.map((name) => ({
      id: null,
      state: null,
      text: name,
      tag: null,
      when: null,
    }));
    return {
      title: 'applications',
      chips: [],
      figure: names.length === 0 ? null : `${names.length}`,
      pieces: listBody(items, 'the server has no applications'),
    };
  }
  if (verb === 'recent') {
    if (!Array.isArray(answer)) return null;
    const items: ForgeListItem[] = [];
    for (const held of answer) {
      const note = obj(held);
      const title = str(note, 'title');
      const app = str(note, 'app');
      if (title === null || app === null) continue;
      const priority = typeof note['priority'] === 'number' ? note['priority'] : null;
      items.push({
        id: null,
        state:
          priority !== null && priority >= 5
            ? { text: `priority ${priority}`, tone: 'warn' }
            : null,
        text: title,
        tag: app,
        when: stamp(str(note, 'date')),
      });
    }
    return {
      title: 'recent notifications',
      chips: [],
      figure: items.length === 0 ? null : `${items.length}`,
      pieces: listBody(items, 'nothing has arrived from the server'),
    };
  }
  return null;
}

/** One gotify subscription off the wire. */
interface GotifyRow {
  id: string | null;
  applications: string[];
  min_priority: number | null;
  names_resolve: boolean;
}

/** One subscription off the wire, or null where a card cannot name it. */
function subOf(value: unknown): GotifyRow | null {
  const held = obj(value);
  const id = str(held, 'id');
  if (id === null) return null;
  const applications = Array.isArray(held['applications'])
    ? held['applications'].filter((name): name is string => typeof name === 'string')
    : [];
  const min_priority = typeof held['min_priority'] === 'number' ? held['min_priority'] : null;
  return {
    id,
    applications,
    min_priority,
    // Absent means the answer carried no degraded case to report.
    names_resolve: held['names_resolve'] !== false,
  };
}

/** The chips a subscription's filter draws: the apps it names and its floor. */
function subscriptionChips(sub: GotifyRow): ForgeChip[] {
  return [
    { text: appsText(sub.applications), tone: 'plain' },
    { text: floorText(sub.min_priority), tone: 'plain' },
  ];
}

/**
 * A list-bearing body: the rows, or the words for finding none.
 *
 * The mock's rule is that a list with no rows draws a line rather than a blank
 * box, so every read that answers with a list goes through here instead of
 * each arm deciding what an empty answer looks like.
 */
function listBody(items: ForgeListItem[], empty: string): ForgePiece[] {
  return items.length === 0 ? [{ kind: 'empty', text: empty }] : [{ kind: 'list', items }];
}

/**
 * The class subscriptions a list answer carries, each with the id it is
 * removed by: no conversation row covers the DM class or the mentions target,
 * so `slack__unsubscribe` cannot be reached for them from the list alone
 * without the handle being on the card.
 */
function classSubscriptions(value: unknown): string[] {
  if (!Array.isArray(value)) return [];
  const out: string[] = [];
  for (const held of value) {
    const row = obj(held);
    const target = str(row, 'target');
    if (target === null || target.trim() === '') continue;
    const id = str(row, 'id');
    out.push(id === null ? target : `${target} (${shortId(id)})`);
  }
  return out;
}

/** The app names a filter matches, or the word for matching any. */
function appsText(applications: string[]): string {
  return applications.length === 0 ? 'any application' : applications.join(', ');
}

/** The priority floor as its own words. */
function floorText(floor: number | null): string {
  return floor === null ? 'any priority' : `priority \u{2265} ${floor}`;
}

/** A uuid as a row spells it: enough to recognise, never the whole thing. */
function shortId(id: string | null): string {
  return id === null ? '' : `${id.slice(0, 4)}\u{2026}`;
}

/** A cron entry as `cron__*` answers it. */
interface CronEntry {
  id: string | null;
  /** The entry's headline: its description, else the prompt's first line. */
  title: string;
  /** The expression, or `once` for a run-once. */
  schedule: string;
  prompt: string;
  next_fire: string | null;
}

/**
 * The cron family's card: create and delete carry an entry, list an array.
 *
 * The chip is the raw expression and the figure is how long until it fires -
 * Ved's shape: no client-side gloss of the expression, because a gloss this
 * page invents is a second reading of the schedule that can disagree with the
 * one the server fired it on.
 */
function cronCard(verb: string, answer: unknown): ForgeCard | null {
  if (verb === 'create') {
    const entry = cronOf(answer);
    if (entry === null) return null;
    const pieces: ForgePiece[] = [{ kind: 'kv', pairs: [['lands in', 'this session']] }];
    const when = cronStamp(entry.next_fire);
    if (when !== null) pieces.push({ kind: 'kv', pairs: [['next fire', when]] });
    pieces.push({ kind: 'quote', text: entry.prompt });
    return {
      title: entry.title,
      chips: [{ text: entry.schedule, tone: 'plain' }],
      figure: cronUntil(entry.next_fire),
      pieces,
    };
  }
  if (verb === 'delete') {
    const envelope = obj(answer);
    if (str(envelope, 'status') !== 'deleted') return null;
    const entry = cronOf(envelope['removed']);
    if (entry === null) return null;
    return {
      title: entry.title,
      chips: [{ text: 'removed', tone: 'dim' }],
      figure: null,
      pieces: [{ kind: 'kv', pairs: [['removed', entry.schedule]] }],
    };
  }
  if (verb === 'list') {
    if (!Array.isArray(answer)) return null;
    const items: ForgeListItem[] = [];
    for (const held of answer) {
      const entry = cronOf(held);
      if (entry === null) return null;
      items.push({
        id: null,
        state: null,
        text: entry.title,
        tag: entry.schedule,
        when: cronUntil(entry.next_fire),
      });
    }
    return {
      title: 'schedules',
      chips: [],
      figure: items.length === 0 ? null : `${items.length} registered`,
      pieces:
        items.length === 0
          ? [{ kind: 'empty', text: 'no schedules registered by this session' }]
          : [{ kind: 'list', items }],
    };
  }
  return null;
}

/** One cron entry off the wire, or null where the fields a card needs are absent. */
function cronOf(value: unknown): CronEntry | null {
  const held = obj(value);
  const prompt = str(held, 'prompt');
  const id = str(held, 'id');
  if (prompt === null || id === null) return null;
  const description = str(held, 'description')?.trim() ?? '';
  return {
    id,
    title: description === '' ? firstLineOf(prompt) : description,
    schedule: scheduleOf(held['schedule']),
    prompt,
    next_fire: str(held, 'next_fire'),
  };
}

/** A cron's schedule as its own words: the expression, or `once` with its time. */
function scheduleOf(schedule: unknown): string {
  const held = obj(schedule);
  const recurring = str(held, 'recurring');
  if (recurring !== null) return recurring;
  const once = str(held, 'once_at');
  if (once !== null) {
    const at = stamp(once);
    return at === null ? 'once' : `once \u{b7} ${at}`;
  }
  return 'scheduled';
}

/** How long until a fire, as the inspector's own countdown reads it. */
function cronUntil(at: string | null): string | null {
  if (at === null) return null;
  const secs = Math.floor(Date.parse(at) / 1000);
  if (Number.isNaN(secs)) return null;
  return `next ${untilOf({ secs_since_epoch: secs }, Date.now())}`;
}

/**
 * A fire's time, local first with UTC beside it: the schedule is authored in
 * the host's zone and the server fires it on UTC, so both are what a reader
 * checking a fire time checks against.
 */
function cronStamp(at: string | null): string | null {
  if (at === null) return null;
  const local = stamp(at);
  if (local === null) return null;
  const parsed = new Date(at);
  const pad = (value: number): string => String(value).padStart(2, '0');
  return `${local} (${pad(parsed.getUTCHours())}:${pad(parsed.getUTCMinutes())} UTC)`;
}

/** A prompt's first line, for a headline when the entry has no description. */
function firstLineOf(text: string): string {
  const line = text.split('\n', 1)[0] ?? '';
  return line.trim() === '' ? text.trim() : line.trim();
}

/** One task as `tasks__*` answers it. */
interface TaskRecord {
  id: string | null;
  subject: string | null;
  status: string | null;
  owner: string | null;
  detail: string | null;
  artifact: string | null;
  estimate: string | null;
  created_at: string | null;
  updated_at: string | null;
}

/**
 * One call's card, or null for every result this module will not dress.
 *
 * A failed call is null by construction: its result is the reason, and the
 * reason draws as every failure does rather than as a card with a missing
 * field.
 */
export function forgeCardOf(
  name: string,
  input: unknown,
  result: { content?: unknown; is_error?: unknown } | undefined,
): ForgeCard | null {
  const family = forgeFamilyOf(name);
  if (family === null) return null;
  // A call still out draws from its own input, for the verbs whose input
  // names the thing it is working on - and for the two the dock can hold for
  // approval, where the wait is itself what the row must say.
  if (result === undefined) return pendingCard(family, verbOf(name), input);
  if (result.is_error === true) return null;
  // A write whose result is one word: the subject is in the input, and the
  // result only confirms that the write went through. A result that said MORE
  // than that word - which nothing emits today - would fall back to the raw
  // text rather than being read for its first block alone.
  if (family === 'slack' && (verbOf(name) === 'edit' || verbOf(name) === 'react')) {
    if (firstText(result.content) === null || trailingTexts(result.content).length > 0) return null;
    return acknowledgementCard(verbOf(name), input);
  }
  // **A declined settings write is a normal answer, not an error.** It
  // settles completed and takes the success tone, so without a face on the
  // row a write the reader turned down reads exactly like one that went
  // through. A chip, not a tail: the tail's two tones are the failure reason
  // and a despawn's `kept`, and the CSS spells those prefixes itself. The
  // tool's own words stay the body.
  if (family === 'settings' && (verbOf(name) === 'set' || verbOf(name) === 'unset')) {
    const said = firstText(result.content);
    if (said !== null && said.startsWith('the reader declined')) {
      const title = settingsTitle(verbOf(name), obj(input));
      if (title === null) return null;
      return {
        title,
        chips: [{ text: 'declined', tone: 'warn' }],
        figure: null,
        pieces: [{ kind: 'text', text: said }],
      };
    }
  }
  // The tasks list is the one verb that opens its result with a clock line,
  // so it reads past it; every other result parses as the plain JSON it is.
  const answer =
    family === 'tasks' && verbOf(name) === 'list'
      ? parsedTextPastClock(result.content)
      : parsedText(result.content);
  const card =
    family === 'voice'
      ? voiceCard(input, firstText(result.content))
      : family === 'tasks'
        ? tasksCard(verbOf(name), input, answer)
        : family === 'cron'
          ? cronCard(verbOf(name), answer)
          : family === 'gotify'
            ? gotifyCard(verbOf(name), answer)
            : family === 'slack'
              ? slackCard(verbOf(name), input, answer)
              : family === 'review'
                ? reviewCard(verbOf(name), input, answer)
                : family === 'documents'
                  ? documentsCard(verbOf(name), input)
                  : family === 'handoff'
                    ? handoffCard(input, answer)
                    : family === 'terminal'
                      ? terminalCard(verbOf(name), input, answer)
                      : family === 'vault'
                        ? vaultCard(verbOf(name), input, answer)
                        : agentsCard(verbOf(name), input, answer);
  // A result that said more than the one block the card reads draws the rest
  // beside the card: the card replaces the body, and a block it swallowed
  // would be the one thing a reader could not find.
  if (card !== null) {
    for (const text of trailingTexts(result.content)) card.pieces.push({ kind: 'text', text });
  }
  return card;
}

/**
 * The review family's card.
 *
 * The anchor is what a reply row is about - `src/chat/units.ts:919` is the
 * thing a reader acts on, and a comment id on its own is not - so the reply
 * and resolve arms are titled by it. A list's chips are the four state words
 * with their counts, each word carrying itself.
 */
/** The noun each family's rows are titled by when the call failed. */
const FAMILY_NOUN: Record<ForgeFamily, string> = {
  tasks: 'tasks',
  cron: 'schedules',
  documents: 'documents',
  gotify: 'notifications',
  slack: 'conversations',
  review: 'reviews',
  settings: 'settings',
  agents: 'workers',
  handoff: 'handoff',
  terminal: 'terminal',
  vault: 'the vault',
  voice: 'speech',
};

/** The last segment of a path, which is the name a row calls a file by. */
function leafName(path: string): string {
  return (
    path
      .split('/')
      .filter((segment) => segment !== '')
      .pop() ?? path
  );
}

/**
 * The settings family's title: which way the call went and what it touched.
 *
 * The family's own noun is the whole of what the glyph says, so a row titled
 * `settings` repeated it and named nothing - a read and a five-key write
 * looked alike collapsed. One or two keys are named; past that the count is
 * what a reader can scan, and the payload behind the open keeps the rest.
 * `null` for an input naming no subject, so the row keeps the family's noun
 * rather than claiming a key nobody named.
 */
function settingsTitle(verb: string, held: Record<string, unknown>): string | null {
  if (verb === 'index') return 'settings \u{b7} read the catalogue';
  const key = str(held, 'key');
  const subject = key === null || key.trim() === '' ? null : key;
  if (verb === 'get') return subject === null ? null : `settings \u{b7} read ${subject}`;
  if (verb === 'unset') return subject === null ? null : `settings \u{b7} clear ${subject}`;
  if (verb !== 'set') return null;
  const edits = held['edits'];
  if (!Array.isArray(edits)) return null;
  const keys = edits.map((edit) => str(obj(edit), 'key'));
  if (keys.some((one) => one === null || one.trim() === '')) return null;
  const named = keys.filter((one): one is string => one !== null);
  if (named.length === 0) return null;
  return named.length <= 2
    ? `settings \u{b7} write ${named.join(', ')}`
    : `settings \u{b7} write ${named.length} keys`;
}

/**
 * A Slack read whose answer the card refuses, titled by what it read.
 *
 * These verbs answer with a message list, a thread's replies, a user search,
 * a transferred file, or the token's identity, none of which the family's
 * card dresses - and with no subject in the title each drew the bare family
 * noun, so a file transfer read as `conversations`. `null` for every verb
 * the card already titles and for an input naming no target, which keeps
 * the family's noun.
 */
function slackFallbackTitle(verb: string, held: Record<string, unknown>): string | null {
  const conversation = str(held, 'conversation');
  switch (verb) {
    case 'me':
      return "read the token's identity";
    case 'history':
      return conversation === null || conversation.trim() === ''
        ? null
        : `read the history of ${conversation}`;
    case 'replies':
      return conversation === null || conversation.trim() === ''
        ? null
        : `read the thread in ${conversation}`;
    case 'users': {
      const query = str(held, 'query');
      return query === null || query.trim() === ''
        ? 'read the users'
        : `searched users for '${query}'`;
    }
    case 'attachment': {
      // **The fetch first, matching the server's own dispatch**: it reads
      // `file_id` before `path`, so a call carrying both fetches.
      const file = str(held, 'file_id');
      if (file !== null && file.trim() !== '') return `downloaded attachment ${file}`;
      const path = str(held, 'path');
      return path === null || path.trim() === '' ? null : `uploaded '${leafName(path)}'`;
    }
    default:
      return null;
  }
}

/**
 * What a forge call's row is titled by when it carries no card.
 *
 * Three cases have no card: a call still out, whose result has not landed; a
 * failed one, whose result IS the reason and draws as every failure does; and
 * a settled one whose answer this module leaves undressed. Each is asked
 * separately, because the same call's tense differs between them: a held
 * upload is `uploading 'brief.pdf'` while it waits and `uploaded 'brief.pdf'`
 * only once it answers. The title comes from the call's own input where that
 * names a subject - in the SUCCESS card's own wording, so a failed write
 * reads `spawned worker 'reviewer'` under a tail that says it failed - and
 * from the family's own noun where the input names nothing. Never the tool:
 * `forge: slack__list` is the plumbing a reader scans past.
 */
export function forgeRowTitle(
  name: string,
  input: unknown,
  outcome: 'pending' | 'settled' | 'failed',
): string | null {
  const family = forgeFamilyOf(name);
  if (family === null) return null;
  const verb = verbOf(name);
  const held = obj(input);
  if (outcome === 'failed') {
    const written = acknowledgementCard(verb, input);
    if (written !== null) return written.title;
    if (verb === 'post') return postTitle(null, str(held, 'conversation'));
    const spawned = verb === 'spawn' ? str(held, 'label') : null;
    if (spawned !== null && spawned.trim() !== '') return `spawned worker '${spawned}'`;
    // A failed vault call reads in the success wording too, under the tail
    // that says it failed.
    const vault = family === 'vault' ? vaultSubject(verb, held, true) : null;
    if (vault !== null) return vault;
  } else if (outcome === 'pending') {
    const pending = pendingCard(family, verb, input);
    if (pending !== null) return pending.title;
  }
  if (family === 'settings') {
    const title = settingsTitle(verb, held);
    if (title !== null) return title;
  }
  if (family === 'slack') {
    const title = slackFallbackTitle(verb, held);
    if (title !== null) return title;
  }
  if (family === 'voice') {
    const spoken = str(held, 'text');
    if (spoken !== null && spoken.trim() !== '') return `speaking \u{b7} ${spoken}`;
  }
  const said = str(held, 'subject') ?? str(held, 'label');
  if (said !== null && said.trim() !== '') return said;
  const query = str(held, 'query');
  if (query !== null && query.trim() !== '') return `search \u{b7} ${query}`;
  return FAMILY_NOUN[family];
}

/**
 * A call still out, drawn from its own input.
 *
 * A held write parks on the dock, and that wait is the one thing the row must
 * not hide: a vault write, a Slack post, edit, react or upload, a settings
 * write and a terminal hand-off all say so. A spawn says what it is bringing
 * up, and a fetch is in flight without being held. Every other verb has
 * nothing to draw until it answers and falls back to its raw text.
 */
function pendingCard(family: ForgeFamily, verb: string, input: unknown): ForgeCard | null {
  const held = obj(input);
  // **A vault call is held on the dock, and that wait is the row's point**:
  // the title names the item it would touch, and the chip says the call has
  // not run yet. The input's secret values are never drawn - only the
  // subject the reader is being asked about.
  if (family === 'vault') {
    const title = vaultSubject(verb, held, false);
    if (title === null) return null;
    return {
      title,
      chips: [{ text: 'waiting for your approval', tone: 'warn' }],
      figure: null,
      pieces: [],
    };
  }
  // **A terminal hand-off waits on the person, and that wait is the row's
  // point**: the ask's reason rides it while it is parked.
  if (family === 'terminal' && verb === 'hand_off') {
    const reason = str(held, 'reason');
    return {
      title: 'handing the terminal to the person',
      chips: [{ text: 'waiting for the person', tone: 'warn' }],
      figure: null,
      pieces: reason === null || reason.trim() === '' ? [] : [{ kind: 'quote', text: reason }],
    };
  }
  // What the call says it is doing, where its input states it: a create or an
  // update names the state it is writing, and the row says it while it waits.
  const chips: ForgeChip[] = [];
  const asked = str(held, 'status');
  if (asked !== null && asked.trim() !== '') chips.push(statusChip(asked));
  if (verb === 'spawn') {
    const label = str(held, 'label');
    if (label === null || label.trim() === '') return null;
    const pieces: ForgePiece[] = [];
    const charter = str(held, 'charter');
    if (charter !== null && charter.trim() !== '') {
      pieces.push({ kind: 'tag', text: 'charter' });
      pieces.push({ kind: 'quote', text: charter });
    }
    return { title: `spawning ${label}`, chips, figure: null, pieces };
  }
  if (verb === 'hand_off') {
    const handed = str(held, 'path');
    if (handed === null || handed.trim() === '') return null;
    return {
      title: `handing the reader ${leafName(handed)}`,
      chips: [{ text: 'waiting for the reader', tone: 'warn' }],
      figure: null,
      pieces: [],
    };
  }
  if (verb === 'post') {
    // The input carries an id and never a name, so the title spells it as
    // given: a `#` on an id would dress it as a channel.
    const channel = str(held, 'conversation');
    if (channel === null || channel.trim() === '') return null;
    const said = str(held, 'text');
    return {
      title: postTitle(null, channel, 'posting'),
      chips: [{ text: 'waiting for your approval', tone: 'warn' }],
      figure: null,
      pieces: said === null || said.trim() === '' ? [] : [{ kind: 'quote', text: said }],
    };
  }
  if (verb === 'edit') {
    const channel = str(held, 'conversation');
    if (channel === null || channel.trim() === '') return null;
    const dropped = held['delete'] === true;
    return {
      title: `${dropped ? 'deleting' : 'updating'} a message in ${channel}`,
      chips: [{ text: 'waiting for your approval', tone: 'warn' }],
      figure: null,
      pieces: [],
    };
  }
  if (verb === 'react') {
    const channel = str(held, 'conversation');
    const name = str(held, 'name');
    if (channel === null || name === null) return null;
    const removed = held['remove'] === true;
    return {
      title: `${removed ? 'removing' : 'reacting'} :${name}: in ${channel}`,
      chips: [{ text: 'waiting for your approval', tone: 'warn' }],
      figure: null,
      pieces: [],
    };
  }
  // **A Slack attachment is told apart by direction and tense**: a fetch is
  // not held but is not done until it answers, and an upload is held on the
  // dock exactly as a post is. The fetch comes first, matching the server's
  // own dispatch on `file_id` before `path`.
  if (family === 'slack' && verb === 'attachment') {
    const file = str(held, 'file_id');
    if (file !== null && file.trim() !== '') {
      return {
        title: `downloading attachment ${file}`,
        chips: [],
        figure: null,
        pieces: [],
      };
    }
    const path = str(held, 'path');
    if (path === null || path.trim() === '') return null;
    return {
      title: `uploading '${leafName(path)}'`,
      chips: [{ text: 'waiting for your approval', tone: 'warn' }],
      figure: null,
      pieces: [],
    };
  }
  // **A settings call is titled by what it touches**, and a write waits on
  // the reader's approval: that wait is the row's point while the call parks.
  if (family === 'settings') {
    const title = settingsTitle(verb, held);
    if (title === null) return null;
    const writes = verb === 'set' || verb === 'unset';
    return {
      title,
      chips: writes ? [{ text: 'waiting for your approval', tone: 'warn' }] : [],
      figure: null,
      pieces: [],
    };
  }
  // A verb with no shape of its own still has what its input names: the
  // subject it is writing, or the state it is writing it in.
  const subject = str(held, 'subject');
  const named = subject === null || subject.trim() === '' ? null : subject;
  if (named === null && chips.length === 0) return null;
  return { title: named ?? FAMILY_NOUN[family], chips, figure: null, pieces: [] };
}

/**
 * A Slack write whose own result is one word.
 *
 * The message it touched and the reaction it set are in the input, so the row
 * is built from those: the result carries no subject at all. The input holds
 * an id and never a name, so the title spells it as given.
 */
function acknowledgementCard(verb: string, input: unknown): ForgeCard | null {
  const held = obj(input);
  const channel = str(held, 'conversation');
  if (channel === null) return null;
  if (verb === 'edit') {
    const dropped = held['delete'] === true;
    const said = str(held, 'text');
    return {
      title: `${dropped ? 'deleted' : 'updated'} a message in ${channel}`,
      chips: [],
      figure: null,
      pieces: dropped || said === null || said.trim() === '' ? [] : [{ kind: 'quote', text: said }],
    };
  }
  const name = str(held, 'name');
  if (name === null) return null;
  const removed = held['remove'] === true;
  const pieces: ForgePiece[] = [];
  const ts = str(held, 'ts');
  if (ts !== null) pieces.push({ kind: 'kv', pairs: [['ts', ts]] });
  return {
    title: `${removed ? 'removed' : 'reacted'} :${name}: in ${channel}`,
    chips: [],
    figure: null,
    pieces,
  };
}

function reviewCard(verb: string, input: unknown, answer: unknown): ForgeCard | null {
  if (verb === 'reply' || verb === 'resolve') {
    const held = obj(answer);
    const file = str(held, 'file');
    const line = typeof held['line'] === 'number' ? held['line'] : null;
    const comment = str(held, 'comment_id');
    if (file === null || comment === null) return null;
    const where = line === null ? file : `${file}:${line}`;
    const status = str(held, 'status') ?? 'unknown';
    const pieces: ForgePiece[] = [];
    const said = str(obj(input), 'text');
    if (said !== null && said.trim() !== '') pieces.push({ kind: 'quote', text: said });
    return {
      title: verb === 'reply' ? `replied on ${where}` : `resolved ${where}`,
      chips: [{ text: status, tone: statusTone(status) }],
      figure: comment,
      pieces,
    };
  }
  if (verb === 'list') {
    if (!Array.isArray(answer)) return null;
    const rows: Record<string, unknown>[] = [];
    for (const held of answer) {
      const review = obj(held);
      if (typeof review['number'] !== 'number') continue;
      rows.push(review);
    }
    const [newest] = rows;
    if (newest === undefined) {
      return {
        title: 'reviews',
        chips: [],
        figure: null,
        pieces: listBody([], 'no reviews on this branch'),
      };
    }
    const number = newest['number'] as number;
    const summary = str(newest, 'summary');
    const head =
      summary === null || summary.trim() === ''
        ? `review #${number}`
        : `review #${number} - ${summary}`;
    const pieces: ForgePiece[] = [];
    const id = str(newest, 'review_id');
    if (id !== null) pieces.push({ kind: 'kv', pairs: [['review id', id]] });
    // Earlier rounds are kept as rows: the newest is what a reader acts on,
    // and the ones under it are the history of the same branch.
    const older: ForgeListItem[] = rows.slice(1).map((row) => ({
      id: null,
      state: null,
      text: str(row, 'summary') ?? `review #${String(row['number'])}`,
      tag: `#${String(row['number'])}`,
      when: dayOf(str(row, 'created_at')),
    }));
    if (older.length > 0) pieces.push({ kind: 'list', items: older });
    return {
      title: head,
      chips: tallyChips(newest),
      figure: dayOf(str(newest, 'created_at')),
      pieces,
    };
  }
  if (verb === 'get') {
    const detail = obj(answer);
    const number = typeof detail['number'] === 'number' ? detail['number'] : null;
    const comments = detail['comments'];
    if (number === null || !Array.isArray(comments)) return null;
    const chips: ForgeChip[] = [];
    // The tally is counted from the comments here and read off the row's own
    // fields for a list, so the two go through ONE order - a detail and a list
    // of the same review must chip their states in the same sequence.
    const counts: Record<string, number> = {};
    for (const held of comments) {
      const status = str(obj(held), 'status') ?? 'unknown';
      counts[status] = (counts[status] ?? 0) + 1;
    }
    chips.push(...tallyChips(counts));
    // Each comment is its own block, the way the mock draws one: the spot, the
    // code it was filed against, and the thread the exchange happened in.
    const blocks: ForgeComment[] = [];
    for (const held of comments) {
      const comment = obj(held);
      const file = str(comment, 'file');
      if (file === null) continue;
      const line = typeof comment['line'] === 'number' ? comment['line'] : null;
      const status = str(comment, 'status') ?? 'unknown';
      blocks.push({
        where: line === null ? file : `${file}:${line}`,
        side: str(comment, 'side') === 'old' ? 'old side' : 'new side',
        state: { text: status, tone: statusTone(status) },
        context: Array.isArray(comment['context'])
          ? comment['context'].filter((one): one is string => typeof one === 'string')
          : [],
        turns: turnsOf(comment['thread']),
      });
    }
    const pieces: ForgePiece[] = [];
    const summary = str(detail, 'summary');
    if (summary !== null && summary.trim() !== '') pieces.push({ kind: 'quote', text: summary });
    if (blocks.length > 0) pieces.push({ kind: 'comments', items: blocks });
    return {
      title: `review #${number}${tallyPhrase(chips) === '' ? '' : ` - ${tallyPhrase(chips)}`}`,
      chips,
      figure:
        comments.length === 0
          ? null
          : `${comments.length} comment${comments.length === 1 ? '' : 's'}`,
      pieces,
    };
  }
  return null;
}

/**
 * The documents family's card: what was handed, by its own name. The tool
 * answers in prose - what the reader's finish filed - so the body beside
 * the card carries that, and the card is the subject.
 */
function documentsCard(verb: string, input: unknown): ForgeCard | null {
  const held = obj(input);
  const handed = str(held, 'path');
  if (verb !== 'hand_off' || handed === null || handed.trim() === '') return null;
  return {
    title: `handed the reader ${leafName(handed)}`,
    chips: [],
    figure: null,
    pieces: [],
  };
}

/** The per-state tally a review answers with, as chips. */
function tallyChips(row: Record<string, unknown>): ForgeChip[] {
  const chips: ForgeChip[] = [];
  for (const status of ['open', 'addressed', 'resolved', 'outdated'] as const) {
    const count = typeof row[status] === 'number' ? row[status] : 0;
    if (count > 0) chips.push({ text: `${count} ${status}`, tone: statusTone(status) });
  }
  return chips;
}

/** The same tally as one phrase, for a title: `2 open, 1 addressed`. */
function tallyPhrase(chips: ForgeChip[]): string {
  return chips.map((chip) => chip.text).join(', ');
}

/** The turns of a comment's thread, with the reviewer read apart from a worker. */
function turnsOf(value: unknown): ForgeComment['turns'] {
  if (!Array.isArray(value)) return [];
  const turns: ForgeComment['turns'] = [];
  for (const held of value) {
    const turn = obj(held);
    const text = str(turn, 'text');
    if (text === null) continue;
    const author = str(turn, 'author') ?? 'you';
    turns.push({ author, text, you: author === 'you' });
  }
  return turns;
}

/** A date as the mock spells one: the month, then the day. */
function dayOf(utc: string | null): string | null {
  if (utc === null) return null;
  const at = new Date(utc);
  if (Number.isNaN(at.getTime())) return null;
  const month = at.toLocaleDateString('en', { month: 'short' }).toLowerCase();
  return `${month} ${at.getDate()}`;
}

/** The chip tone a review state's word draws. */
function statusTone(status: string): ForgeChip['tone'] {
  switch (status) {
    case 'open':
      return 'bad';
    case 'addressed':
      return 'info';
    case 'resolved':
      return 'ok';
    case 'outdated':
      return 'warn';
    default:
      return 'plain';
  }
}

/**
 * The agents family's card: the spawn echo the server answers with.
 *
 * The label is the call's own input - the result names the session, not the
 * worker - so the row is titled by what the lead asked for and the row the
 * server echoed rides the body beside where the worker landed.
 */
function agentsCard(verb: string, input: unknown, answer: unknown): ForgeCard | null {
  if (verb === 'update') return updateCard(input, answer);
  if (verb === 'capacity') return capacityCard(answer);
  if (verb === 'despawn') return despawnCard(input, answer);
  if (verb !== 'spawn') return null;
  const held = obj(answer);
  const session = str(held, 'session_id');
  if (session === null) return null;
  const label = str(obj(input), 'label') ?? 'worker';
  const chips: ForgeChip[] = [
    held['resumed'] === true ? { text: 'resumed', tone: 'info' } : { text: 'fresh', tone: 'plain' },
  ];
  const families = held['mcp_families'];
  if (Array.isArray(families)) {
    chips.push({
      text: families.length === 0 ? 'all families' : families.join(', '),
      tone: 'dim',
    });
  } else {
    chips.push({ text: 'all families', tone: 'dim' });
  }
  const pieces: ForgePiece[] = [];
  const worktree = str(held, 'worktree');
  if (worktree !== null) pieces.push({ kind: 'kv', pairs: [['worktree', worktree]] });
  const charter = str(obj(input), 'charter');
  if (charter !== null && charter.trim() !== '') {
    pieces.push({ kind: 'tag', text: 'charter' });
    pieces.push({ kind: 'quote', text: charter });
  }
  const kick = str(obj(input), 'kick');
  if (kick !== null && kick.trim() !== '') {
    pieces.push({ kind: 'tag', text: 'kick' });
    pieces.push({ kind: 'quote', text: kick });
  }
  const warning = str(held, 'durability_warning');
  if (warning !== null) {
    pieces.push({ kind: 'warnline', label: 'durability', text: warning });
  }
  const notice = str(held, 'notice');
  if (notice !== null) {
    pieces.push({ kind: 'warnline', label: 'account', text: notice });
  }
  return {
    title: `spawned worker '${label}'`,
    chips,
    figure: session.slice(0, 6),
    pieces,
  };
}

/**
 * The voice family's card: one utterance, and what became of it.
 *
 * The spoken words are the subject, so the row is titled by them - the closed
 * row ellipsises them and opening it reads the whole - and the result's own
 * word says whether anyone heard it.
 */
function voiceCard(input: unknown, answer: string | null): ForgeCard | null {
  const said = str(obj(input), 'text');
  if (said === null || said.trim() === '') return null;
  const status = answer === null ? '' : answer.trim();
  const chips: ForgeChip[] = [];
  if (status === 'spoken: queued') chips.push({ text: status, tone: 'ok' });
  else if (status === 'no listener') chips.push({ text: status, tone: 'warn' });
  else if (status !== '') chips.push({ text: status, tone: 'plain' });
  return {
    title: `speaking \u{b7} ${said}`,
    chips,
    figure: null,
    pieces: [{ kind: 'quote', text: said }],
  };
}

/** The last `lines` of a screen or a read, which is what a terminal row can
 *  show without swallowing the whole buffer. */
function screenTail(text: string | null, lines = 12): string | null {
  if (text === null) return null;
  const trimmed = text.replace(/\s+$/, '');
  if (trimmed === '') return null;
  const all = trimmed.split('\n');
  return all.slice(Math.max(0, all.length - lines)).join('\n');
}

/** The shell's state in words: what it is doing, and how it ended. */
function terminalChips(state: unknown): ForgeChip[] {
  const held = obj(state);
  const chips: ForgeChip[] = [];
  if (held['running'] === true) chips.push({ text: 'running', tone: 'info' });
  const expected = obj(held['input_expected']);
  const kind = str(expected, 'kind');
  if (kind === 'password' || kind === 'confirm') {
    chips.push({ text: 'waiting for input', tone: 'warn' });
  }
  const exit = obj(held['exit']);
  const code = exit['code'];
  if (typeof code === 'number') {
    chips.push({ text: `exit ${code}`, tone: code === 0 ? 'plain' : 'bad' });
  }
  return chips;
}

/**
 * The terminal family's card: the terminal a call acted on, with the shell's
 * state in words, and the screen or the command's own text as its body.
 */
function terminalCard(verb: string, input: unknown, answer: unknown): ForgeCard | null {
  const asked = obj(input);
  const held = obj(answer);
  const id =
    typeof held['id'] === 'number'
      ? held['id']
      : typeof asked['id'] === 'number'
        ? asked['id']
        : null;
  const name = id === null ? 'the terminal' : `terminal ${id}`;
  const chips = terminalChips(held['state']);
  const pieces: ForgePiece[] = [];
  switch (verb) {
    case 'open': {
      const shell = str(held, 'shell');
      const cwd = str(held, 'cwd');
      const where = [shell, cwd].filter((part): part is string => part !== null && part !== '');
      if (where.length > 0) pieces.push({ kind: 'tag', text: where.join(' · ') });
      const tail = screenTail(str(held, 'screen'));
      if (tail !== null) pieces.push({ kind: 'quote', text: tail });
      return { title: `opened ${name}`, chips, figure: null, pieces };
    }
    case 'write':
      return { title: `typed into ${name}`, chips, figure: null, pieces };
    case 'read': {
      const tail = screenTail(str(held, 'text'));
      if (tail !== null) pieces.push({ kind: 'quote', text: tail });
      if (held['truncated'] === true) chips.push({ text: 'truncated', tone: 'dim' });
      return { title: `read ${name}`, chips, figure: null, pieces };
    }
    case 'wait': {
      const tail = screenTail(str(held, 'text'));
      if (tail !== null) pieces.push({ kind: 'quote', text: tail });
      if (held['matched'] === true) chips.push({ text: 'matched', tone: 'ok' });
      return { title: `waited on ${name}`, chips, figure: null, pieces };
    }
    case 'run': {
      const tail = screenTail(str(held, 'text'));
      if (tail !== null) pieces.push({ kind: 'quote', text: tail });
      if (held['truncated'] === true) chips.push({ text: 'truncated', tone: 'dim' });
      const exit = obj(held['exit']);
      const code = exit['code'];
      if (typeof code === 'number') {
        chips.push({ text: `exit ${code}`, tone: code === 0 ? 'ok' : 'bad' });
      }
      return { title: 'ran a command in a terminal', chips, figure: null, pieces };
    }
    case 'resize': {
      const cols = typeof asked['cols'] === 'number' ? asked['cols'] : null;
      const rows = typeof asked['rows'] === 'number' ? asked['rows'] : null;
      if (cols !== null && rows !== null) pieces.push({ kind: 'tag', text: `${cols}×${rows}` });
      return { title: `resized ${name}`, chips, figure: null, pieces };
    }
    case 'close': {
      const exit = obj(held['exit']);
      const code = exit['code'];
      if (typeof code === 'number') chips.push({ text: `exit ${code}`, tone: 'plain' });
      return { title: `closed ${name}`, chips, figure: null, pieces };
    }
    case 'hand_off': {
      const ending = str(held, 'ending');
      if (ending !== null && ending.trim() !== '') {
        chips.push({ text: ending, tone: 'ok' });
      }
      return { title: `handed ${name} to the person`, chips, figure: null, pieces };
    }
    default:
      return null;
  }
}

/**
 * The title a vault row carries, in the tense the call's own state calls for:
 * a held call is doing the thing, a failed one reads in the success wording
 * under a tail that says it failed.
 *
 * `null` for a call whose input names no subject, so the row falls back to
 * the family's noun rather than claiming an item nobody named.
 */
function vaultSubject(verb: string, held: Record<string, unknown>, done: boolean): string | null {
  const id = str(held, 'id');
  const name = str(held, 'name');
  const item = id === null ? 'the item' : `item ${id}`;
  switch (verb) {
    case 'create': {
      if (name === null || name.trim() === '') return null;
      const kind = str(held, 'type') ?? 'login';
      return `${done ? 'created' : 'creating'} ${kind} '${name}'`;
    }
    case 'update':
      return `${done ? 'updated' : 'updating'} ${item}`;
    case 'get':
      return `${done ? 'read' : 'reading'} ${item}`;
    case 'trash':
      return `${done ? 'moved' : 'moving'} ${item} to the trash`;
    case 'restore':
      return `${done ? 'restored' : 'restoring'} ${item}`;
    case 'totp_code':
      return `${done ? 'computed' : 'computing'} a TOTP code for ${item}`;
    case 'search': {
      const query = str(held, 'query');
      const forWhat = query === null || query.trim() === '' ? '' : ` for '${query}'`;
      return `${done ? 'searched' : 'searching'} the vault${forWhat}`;
    }
    case 'attachment_get': {
      const of = str(held, 'item_id');
      return `${done ? 'downloaded' : 'downloading'} an attachment from ${of === null ? 'the item' : `item ${of}`}`;
    }
    case 'attachment_put': {
      const path = str(held, 'path');
      return `${done ? 'uploaded' : 'uploading'} ${path === null ? 'a file' : `'${leafName(path)}'`}`;
    }
    case 'folder_list':
      return done ? 'listed the folders' : 'listing folders';
    case 'folder_create':
      return name === null || name.trim() === ''
        ? null
        : `${done ? 'created' : 'creating'} folder '${name}'`;
    case 'folder_update':
      return name === null || name.trim() === ''
        ? null
        : `${done ? 'renamed' : 'renaming'} folder '${name}'`;
    case 'folder_delete':
      return `${done ? 'deleted' : 'deleting'} folder ${id ?? ''}`.trim();
    default:
      return null;
  }
}

/** A vault item's type, in the word the family's own results use for it. */
function itemKind(value: unknown): string | null {
  if (typeof value === 'number') {
    if (value === 1) return 'login';
    if (value === 2) return 'note';
    if (value === 3) return 'card';
    return null;
  }
  return typeof value === 'string' && value !== '' ? value : null;
}

/** One revealed field's value as words: a value, or a card's expiry pair. */
function shownValue(value: unknown): string | null {
  if (typeof value === 'string') return value;
  if (typeof value === 'number') return String(value);
  const held = obj(value);
  const month = str(held, 'expMonth');
  const year = str(held, 'expYear');
  return month === null || year === null ? null : `${month}/${year}`;
}

/**
 * The vault family's card: which item a call touched, and what it found.
 *
 * The result's own fields name the subject, so the card is built from the
 * ANSWER and never the input: an update's input is a patch, whose values are
 * the secrets a card must not draw. `search` and `folder_list` answer rows
 * and draw them as the family's list; a reveal draws the one value the caller
 * asked for, because a card replaces the raw body that would otherwise hold
 * it.
 */
function vaultCard(verb: string, input: unknown, answer: unknown): ForgeCard | null {
  const asked = obj(input);
  const held = obj(answer);
  switch (verb) {
    case 'search': {
      const items = held['items'];
      if (!Array.isArray(items)) return null;
      const rows: ForgeListItem[] = [];
      for (const raw of items) {
        const item = obj(raw);
        const name = str(item, 'name');
        if (name === null) continue;
        rows.push({
          id: null,
          state: null,
          text: name,
          tag: str(item, 'username'),
          when: dayOf(str(item, 'updated_at')),
        });
      }
      const query = str(asked, 'query');
      const title =
        query === null || query.trim() === ''
          ? 'searched the vault'
          : `searched the vault for '${query}'`;
      return {
        title,
        chips: [{ text: `${rows.length} ${rows.length === 1 ? 'hit' : 'hits'}`, tone: 'plain' }],
        figure: null,
        pieces: listBody(rows, 'nothing matched'),
      };
    }
    case 'get': {
      const name = str(held, 'name');
      if (name === null) return null;
      const chips: ForgeChip[] = [];
      const kind = itemKind(held['type']);
      if (kind !== null) chips.push({ text: kind, tone: 'plain' });
      if (str(held, 'deleted_date') !== null) chips.push({ text: 'trashed', tone: 'dim' });
      if (held['favorite'] === true) chips.push({ text: 'favorite', tone: 'dim' });
      const pairs: [string, string][] = [];
      const folder = str(held, 'folder_id');
      if (folder !== null) pairs.push(['folder id', folder]);
      const username = str(held, 'username');
      if (username !== null) pairs.push(['username', username]);
      const uris = held['uris'];
      if (Array.isArray(uris)) {
        const list = uris.filter((uri): uri is string => typeof uri === 'string');
        if (list.length > 0) pairs.push(['uri', list.join(' \u{b7} ')]);
      }
      const pieces: ForgePiece[] = [];
      if (pairs.length > 0) pieces.push({ kind: 'kv', pairs });
      const revealed = obj(held['revealed']);
      const field = str(revealed, 'field');
      const value = shownValue(revealed['value']);
      if (field !== null && value !== null) pieces.push({ kind: 'kv', pairs: [[field, value]] });
      return { title: `read '${name}'`, chips, figure: null, pieces };
    }
    case 'create':
    case 'update':
    case 'trash':
    case 'restore':
    case 'folder_create':
    case 'folder_update':
    case 'folder_delete': {
      const title = vaultSubject(verb, { ...asked, ...held }, true);
      if (title === null) return null;
      const id = str(held, 'id');
      return {
        title,
        chips: [],
        figure: null,
        pieces: id === null ? [] : [{ kind: 'kv', pairs: [['id', id]] }],
      };
    }
    case 'totp_code': {
      const code = str(held, 'code');
      if (code === null) return null;
      const chips: ForgeChip[] = [{ text: code, tone: 'plain' }];
      const period = countOf(held['period']);
      if (period !== null) chips.push({ text: `period ${period}s`, tone: 'dim' });
      const remaining = countOf(held['remaining']);
      return {
        title: 'computed a TOTP code',
        chips,
        figure: remaining === null ? null : `${remaining}s left`,
        pieces: [],
      };
    }
    case 'attachment_get': {
      const path = str(held, 'path');
      if (path === null) return null;
      const pairs: [string, string][] = [['path', path]];
      const size = countOf(held['size']);
      if (size !== null) pairs.push(['size', `${size} bytes`]);
      const sha = str(held, 'sha256');
      if (sha !== null) pairs.push(['sha256', sha]);
      return {
        title: 'downloaded an attachment',
        chips: [],
        figure: null,
        pieces: [{ kind: 'kv', pairs }],
      };
    }
    case 'attachment_put': {
      const name = str(held, 'name');
      if (name === null) return null;
      const chips: ForgeChip[] = [];
      if (str(held, 'replaced') !== null) chips.push({ text: 'replaced', tone: 'dim' });
      const pairs: [string, string][] = [];
      const size = countOf(held['size']);
      if (size !== null) pairs.push(['size', `${size} bytes`]);
      const sha = str(held, 'sha256');
      if (sha !== null) pairs.push(['sha256', sha]);
      return {
        title: `uploaded '${name}'`,
        chips,
        figure: null,
        pieces: pairs.length === 0 ? [] : [{ kind: 'kv', pairs }],
      };
    }
    case 'folder_list': {
      const folders = held['folders'];
      if (!Array.isArray(folders)) return null;
      const rows: ForgeListItem[] = [];
      for (const raw of folders) {
        const folder = obj(raw);
        const name = str(folder, 'name');
        if (name === null) continue;
        rows.push({ id: str(folder, 'id'), state: null, text: name, tag: null, when: null });
      }
      return {
        title: 'folders',
        chips: [{ text: `${rows.length}`, tone: 'plain' }],
        figure: null,
        pieces: listBody(rows, 'no folders'),
      };
    }
    default:
      return null;
  }
}

/**
 * The handoff's exit call: the note it carried is what a reader wants to see,
 * and it lives in the call's input, not its answer.
 */
function handoffCard(input: unknown, answer: unknown): ForgeCard | null {
  const note = str(obj(input), 'brief');
  const held = obj(answer);
  const status = str(held, 'status');
  const runs = str(held, 'runs');
  const hasNote = note !== null && note.trim() !== '';
  if (!hasNote && status === null) return null;
  const chips: ForgeChip[] = [];
  if (status !== null && status.trim() !== '') chips.push({ text: status, tone: 'info' });
  if (runs !== null && runs.trim() !== '') chips.push({ text: runs, tone: 'dim' });
  const pieces: ForgePiece[] = [];
  if (hasNote) {
    pieces.push({ kind: 'tag', text: 'note' });
    pieces.push({ kind: 'quote', text: note });
  }
  return { title: 'handed the seat over', chips, figure: null, pieces };
}

/**
 * A worker whose fields moved: the label and the field names the server says
 * it wrote. The values are not echoed - only the names - so the row chips the
 * names and says when they take effect rather than inventing what they became.
 */
function updateCard(input: unknown, answer: unknown): ForgeCard | null {
  const held = obj(answer);
  const label = str(held, 'label') ?? str(obj(input), 'label');
  const updated = held['updated'];
  if (label === null || !Array.isArray(updated)) return null;
  const fields = updated.filter((field): field is string => typeof field === 'string');
  return {
    title: `updated worker '${label}'`,
    chips: fields.map((field) => ({ text: field, tone: 'plain' as const })),
    figure: 'next respawn',
    pieces: [],
  };
}

/**
 * The pool as it stands against its cap, with the meter carrying the one
 * proportion a reader wants: the two numbers are the chips, the bar is them.
 */
function capacityCard(answer: unknown): ForgeCard | null {
  const held = obj(answer);
  const cap = countOf(held['cap']);
  const live = countOf(held['live']);
  if (cap === null || live === null) return null;
  const source = str(held, 'cap_source');
  const pairs: [string, string][] = [];
  const project = str(held, 'project');
  if (project !== null) pairs.push(['project', project]);
  if (source !== null) pairs.push(['cap source', source]);
  const available = countOf(held['available']);
  const from = source === 'max_workers' ? 'forge.toml' : source;
  return {
    title: 'worker capacity',
    chips: [
      { text: `${live} live`, tone: 'plain' },
      { text: from === null ? `cap ${cap}` : `cap ${cap} \u{b7} ${from}`, tone: 'dim' },
    ],
    figure: available === null ? null : `${available} free`,
    pieces: pairs.length === 0 ? [] : [{ kind: 'kv', pairs }],
    meter: { fill: live, of: cap },
    glyph: 'gauge',
  };
}

/**
 * A worker taken out of the pool, or the refusal that stopped it.
 *
 * A refusal is a RESULT and not a failed call - the tool answered cleanly that
 * the worker stays up - so the row carries the reason on its own tail, where a
 * failed call carries its error, and says the same in words in the body.
 */
function despawnCard(input: unknown, answer: unknown): ForgeCard | null {
  const held = obj(answer);
  const status = str(held, 'status');
  const label = str(obj(input), 'label') ?? 'worker';
  if (status === 'blocked') {
    const reason = str(held, 'reason') ?? 'the worker could not be closed';
    return {
      title: `worker '${label}' still live`,
      chips: [],
      figure: null,
      pieces: [{ kind: 'warnline', label: 'blocked', text: reason }],
      tail: { text: reason, tone: 'warn' },
    };
  }
  if (status !== 'despawned') return null;
  const pieces: ForgePiece[] = [];
  let worktreeRemoved = true;
  for (const [field, name] of [
    ['worktree_cleanup_warning', 'worktree'],
    ['branch_cleanup_warning', 'branch'],
  ] as const) {
    const warning = str(held, field);
    if (warning === null) continue;
    if (field === 'worktree_cleanup_warning') worktreeRemoved = false;
    pieces.push({ kind: 'warnline', label: name, text: warning });
  }
  return {
    title: `closed worker '${label}'`,
    // The chip is the factual half of the row and never contradicts the body:
    // the server sends a worktree warning exactly when the teardown failed.
    chips: worktreeRemoved ? [{ text: 'worktree removed', tone: 'dim' }] : [],
    figure: null,
    pieces,
  };
}

/**
 * The slack family's card: what was subscribed, what went out, what a read
 * found.
 *
 * A channel is named by its name and never by its id - the sheet's own rule -
 * and a target the server could not resolve yet draws the id it knows rather
 * than a name this page would have to invent.
 */
function slackCard(verb: string, input: unknown, answer: unknown): ForgeCard | null {
  if (verb === 'subscribe') {
    if (!Array.isArray(answer)) return null;
    const chips: ForgeChip[] = [];
    const items: ForgeListItem[] = [];
    let workspace: string | null = null;
    for (const held of answer) {
      const row = obj(held);
      const target = targetOf(row['target']);
      if (target === null) return null;
      workspace ??= str(row, 'workspace');
      chips.push(target.chip);
      items.push({
        id: null,
        state: null,
        text: target.text,
        tag: null,
        when: shortId(str(row, 'id')),
      });
    }
    // **The workspace IS the address on Slack**: one channel name can exist in
    // two workspaces, so a row that named only the channel would be ambiguous.
    return {
      title: workspace === null ? 'subscribed in Slack' : `subscribed in ${workspace}`,
      chips,
      figure: items.length === 0 ? null : `${items.length}`,
      pieces: listBody(items, 'nothing was subscribed'),
    };
  }
  if (verb === 'unsubscribe') {
    const envelope = obj(answer);
    if (str(envelope, 'status') !== 'deleted') return null;
    const row = obj(envelope['removed']);
    const target = targetOf(row['target']);
    if (target === null) return null;
    const workspace = str(row, 'workspace');
    return {
      title: workspace === null ? 'unsubscribed' : `unsubscribed in ${workspace}`,
      chips: [{ ...target.chip, tone: 'dim' }],
      figure: null,
      pieces: [],
    };
  }
  if (verb === 'post') {
    const ts = tsOf(answer);
    if (ts === null) return null;
    const held = obj(answer);
    const name = str(held, 'conversation_name');
    const parts = typeof held['parts'] === 'number' ? held['parts'] : null;
    // The resolved label rides the result: the call takes no workspace argument.
    const workspace = str(held, 'workspace');
    const where = name === null ? null : channelName(name);
    return {
      title: postTitle(workspace, where),
      chips: parts === null || parts <= 1 ? [] : [{ text: `${parts} parts`, tone: 'plain' }],
      figure: ts.length === 0 ? null : `ts ${ts[0]}`,
      pieces: [],
    };
  }
  if (verb === 'search') {
    const found = obj(answer);
    const matches = found['matches'];
    if (!Array.isArray(matches)) return null;
    const items: ForgeListItem[] = [];
    for (const held of matches) {
      const hit = obj(held);
      const text = str(hit, 'text');
      if (text === null) continue;
      const channel = str(hit, 'conversation_name');
      const user = str(hit, 'username');
      items.push({
        id: null,
        state: null,
        text,
        tag: channel === null || channel === '' ? null : channelName(channel),
        when: user,
      });
    }
    const query = str(obj(input), 'query');
    const what = query === null || query.trim() === '' ? 'search' : `search \u{b7} "${query}"`;
    return {
      title: withWorkspace(what, str(found, 'workspace')),
      chips: [],
      figure: items.length === 0 ? null : `${items.length} hits`,
      pieces: listBody(items, 'no messages matched the search'),
    };
  }
  if (verb === 'list') {
    const listed = obj(answer);
    const conversations = listed['conversations'];
    if (!Array.isArray(conversations)) return null;
    const items: ForgeListItem[] = [];
    for (const held of conversations) {
      const row = obj(held);
      const name = str(row, 'name');
      const kind = str(row, 'kind');
      items.push({
        id: null,
        state: row['subscribed'] === true ? { text: 'watching', tone: 'ok' } : null,
        text: name === null ? 'conversation' : channelName(name),
        tag: kind,
        when: null,
      });
    }
    const pieces = listBody(items, 'no conversations this token can see');
    // The class subscriptions - the DM class and the workspace mention target
    // - cover no single conversation, so they ride no row of the list; their
    // ids are what `slack__unsubscribe` takes, so the card names them.
    const classes = classSubscriptions(listed['subscriptions']);
    if (classes.length > 0) {
      pieces.push({ kind: 'kv', pairs: [['also watching', classes.join(', ')]] });
    }
    return {
      title: withWorkspace('conversations', str(listed, 'workspace')),
      chips: [],
      figure: items.length === 0 ? null : `${items.length}`,
      pieces,
    };
  }
  if (verb === 'pins' || verb === 'bookmarks') {
    const listed = obj(answer);
    const kept = listed[verb];
    if (!Array.isArray(kept)) return null;
    const items: ForgeListItem[] = [];
    for (const held of kept) {
      const row = obj(held);
      const text = str(row, verb === 'pins' ? 'text' : 'title') ?? str(row, 'link');
      if (text === null) continue;
      items.push({
        id: null,
        state: null,
        text,
        tag: null,
        when: verb === 'pins' ? str(row, 'user') : str(row, 'link'),
      });
    }
    const where = str(obj(input), 'conversation');
    const title = where === null ? verb : `${verb} in ${where}`;
    return {
      title: withWorkspace(title, str(listed, 'workspace')),
      chips: [],
      figure: items.length === 0 ? null : `${items.length}`,
      pieces: listBody(items, `no ${verb} in this conversation`),
    };
  }
  if (verb === 'user') {
    const user = str(obj(answer), 'name');
    if (user === null) return null;
    const pairs: [string, string][] = [];
    for (const key of ['real_name', 'tz']) {
      const said = str(obj(answer), key);
      if (said !== null && said !== '') pairs.push([key.replace('_', ' '), said]);
    }
    return {
      title: `user ${user}`,
      chips: [],
      figure: null,
      pieces: pairs.length === 0 ? [] : [{ kind: 'kv', pairs }],
    };
  }
  return null;
}

/** A slack target as its own words: a channel with its mode, the DM class, or mentions. */
function targetOf(value: unknown): { chip: ForgeChip; text: string } | null {
  const held = obj(value);
  const kind = str(held, 'kind');
  if (kind === 'conversation') {
    const id = str(held, 'id');
    if (id === null) return null;
    const name = str(held, 'name');
    const mode = str(held, 'mode') === 'mentions' ? 'mentions' : 'all';
    const label = name === null ? id : channelName(name);
    return { chip: { text: `${label} \u{b7} ${mode}`, tone: 'info' }, text: label };
  }
  if (kind === 'dm') return { chip: { text: 'DMs', tone: 'plain' }, text: 'direct messages' };
  if (kind === 'mentions') {
    return { chip: { text: 'mentions', tone: 'plain' }, text: 'mentions anywhere' };
  }
  return null;
}

/** A conversation's name, as the sheet spells a channel: the marker only on a name. */
function channelName(name: string): string {
  if (name === '' || name.startsWith('#') || name.startsWith('@')) return name;
  // An id is never dressed as a channel: a DM's name falls back to the
  // partner's user id, and `#U0AE0CBJ77G` reads as a channel that does not
  // exist. The same heuristic the envelope's author clause uses.
  return isSlackId(name) ? name : `#${name}`;
}

/** A read's title, naming the workspace the result says it came from. */
function withWorkspace(title: string, workspace: string | null): string {
  return workspace === null ? title : `${title} \u{b7} ${workspace}`;
}

/** A post's title: the workspace first, then the channel, whichever are known. */
function postTitle(workspace: string | null, channel: string | null, lead = 'posted'): string {
  if (workspace !== null && channel !== null) return `${lead} to ${workspace} \u{b7} ${channel}`;
  if (channel !== null) return `${lead} to ${channel}`;
  return workspace === null ? `${lead} to Slack` : `${lead} to ${workspace}`;
}

/** The ts list a post answered with, or null for an answer that is not one. */
function tsOf(answer: unknown): string[] | null {
  const held = obj(answer)['ts'];
  if (!Array.isArray(held)) return null;
  return held.filter((one): one is string => typeof one === 'string');
}

/**
 * A tool name's own verb, the last segment of `mcp__<server>__<family>__<verb>`.
 *
 * The family segment is what picks the card; the verb picks the arm inside it.
 */
function verbOf(name: string): string {
  const parts = name.split('__');
  return parts.length === 0 ? '' : (parts[parts.length - 1] ?? '');
}

/**
 * The tasks family's card: create / update / claim / wait echo a record,
 * delete an envelope, list an array.
 */
function tasksCard(verb: string, input: unknown, answer: unknown): ForgeCard | null {
  if (verb === 'create' || verb === 'update' || verb === 'claim') {
    const record = taskOf(answer);
    if (record === null || record.subject === null) return null;
    const pieces: ForgePiece[] = [];
    // The detail is prose, so it draws as prose: the meta line under it is
    // for the record's own facts, the way an instance's row splits its brief
    // from its facts.
    if (record.detail !== null && record.detail !== '') {
      pieces.push({ kind: 'quote', text: record.detail });
    }
    pieces.push({
      kind: 'kv',
      pairs: [
        // The patch line is an update's own: its input names the fields it
        // moved. A claim's input named the row and nothing else, so the
        // record's own state is the whole of what it says.
        ...namedBy(input, verb === 'update'),
        ...recordFacts(record, verb === 'create' ? 'created_at' : 'updated_at'),
      ],
    });
    return {
      title: record.subject,
      chips: [statusChip(record.status)],
      figure: record.owner === null ? null : `owner ${record.owner}`,
      pieces,
    };
  }
  if (verb === 'wait') {
    const record = taskOf(answer);
    if (record === null || record.subject === null) return null;
    const pieces: ForgePiece[] = [];
    if (record.detail !== null && record.detail !== '') {
      pieces.push({ kind: 'quote', text: record.detail });
    }
    // The words the caller left with the wait: the tool's echo carries the
    // record and not its `waiting_on`, so what the row now waits on is read
    // off the call's own input - the same statement the server wrote.
    const said = str(obj(input), 'detail');
    if (said !== null && said.trim() !== '') {
      pieces.push({ kind: 'tag', text: 'the wait' });
      pieces.push({ kind: 'quote', text: said });
    }
    pieces.push({ kind: 'kv', pairs: recordFacts(record, 'updated_at') });
    return {
      title: record.subject,
      chips: [waitChip(input, record)],
      figure: record.owner === null ? null : `owner ${record.owner}`,
      pieces,
    };
  }
  if (verb === 'delete') {
    const envelope = obj(answer);
    if (str(envelope, 'status') !== 'deleted') return null;
    const record = taskOf(envelope['removed']);
    if (record === null || record.subject === null) return null;
    const removed = countOf(envelope['descendants_removed']);
    return {
      title: record.subject,
      chips: [{ text: 'removed', tone: 'dim' }],
      figure:
        removed === null || removed === 0
          ? null
          : `with ${removed} subtask${removed === 1 ? '' : 's'}`,
      pieces: [],
    };
  }
  if (verb === 'list') {
    if (!Array.isArray(answer)) return null;
    const items: ForgeListItem[] = [];
    for (const held of answer) {
      const record = taskOf(held);
      if (record === null || record.subject === null) return null;
      items.push({
        id: record.id,
        state: statusChip(record.status),
        text: record.subject,
        tag: null,
        when: taskWhen(record),
      });
    }
    return {
      title: 'tasks',
      chips: [],
      figure: items.length === 0 ? null : `${items.length} in flight`,
      pieces:
        items.length === 0
          ? [
              {
                kind: 'empty',
                text: 'no tasks in flight - anything this project declares lands here',
              },
            ]
          : [{ kind: 'list', items }],
    };
  }
  return null;
}

/**
 * The fields the call itself stated, as the meta line's leading pairs.
 *
 * An update's own patch is the part the echo cannot show - a record says where
 * it stands, never what moved - so the changed field names lead. A create
 * states the whole task, and those values are on the record already.
 */
function namedBy(input: unknown, changed: boolean): [string, string][] {
  if (!changed) return [];
  const stated = obj(input);
  const names = Object.keys(stated).filter(
    (key) => key !== 'id' && stated[key] !== undefined && stated[key] !== null,
  );
  // Raw, the way the update card's own chips spell a field: one spelling for
  // a field name across the family, and it is the server's.
  return names.length === 0 ? [] : [['changed', names.join(', ')]];
}

/** The record's own facts, as pairs: what it estimates, points at, and when it moved. */
function recordFacts(
  record: TaskRecord,
  stampKey: 'created_at' | 'updated_at',
): [string, string][] {
  const rows: [string, string][] = [];
  for (const [label, said] of [
    ['owner', record.owner],
    ['estimate', record.estimate],
    ['artifact', record.artifact],
  ] as [string, string | null][]) {
    if (said !== null && said !== '') rows.push([label, said]);
  }
  const at = stamp(record[stampKey]);
  if (at !== null) rows.push([stampKey === 'created_at' ? 'created' : 'updated', at]);
  return rows;
}

/** One task record off the wire, or null where the fields a card needs are absent. */
function taskOf(value: unknown): TaskRecord | null {
  const held = obj(value);
  // The id is what every arm names the task by; a JSON object without one is
  // not a task record, whatever else it carries.
  if (str(held, 'id') === null && str(held, 'subject') === null) return null;
  return {
    id: str(held, 'id'),
    subject: str(held, 'subject'),
    status: str(held, 'status'),
    owner: str(held, 'owner'),
    detail: str(held, 'detail'),
    artifact: str(held, 'artifact'),
    estimate: str(held, 'estimate'),
    created_at: str(held, 'created_at'),
    updated_at: str(held, 'updated_at'),
  };
}

/**
 * A task list row's trailing column: who holds it and how long it is, with
 * the artifact standing in for a holder that has none - an unheld task says
 * so rather than leaving the column empty.
 */
function taskWhen(record: TaskRecord): string | null {
  const parts: string[] = [];
  if (record.owner !== null && record.owner.trim() !== '') parts.push(record.owner);
  if (record.estimate !== null && record.estimate.trim() !== '') parts.push(record.estimate);
  if (parts.length === 0) parts.push(record.artifact ?? 'unclaimed');
  return parts.join(' \u{b7} ');
}

/**
 * The chip a waiting row carries: what it waits on, in the board's own words.
 *
 * A test holds these three phrases equal to the board's (`board/view.ts`), so
 * one wait reads the same on either surface. A call whose kind the page
 * cannot read keeps the record's own state word, so the chip is never blank.
 */
function waitChip(input: unknown, record: TaskRecord): ForgeChip {
  const held = obj(input);
  const kind = str(held, 'kind');
  if (kind === 'decision') return { text: 'waiting on you', tone: 'warn' };
  if (kind === 'dependency') {
    return { text: `on ${str(held, 'on') ?? 'another row'}`, tone: 'warn' };
  }
  if (kind === 'resource') return { text: 'waiting on a resource', tone: 'warn' };
  return statusChip(record.status);
}

/** The chip a task's status draws: the word the wire sends, spaced and toned. */
function statusChip(status: string | null): ForgeChip {
  switch (status) {
    case 'in_progress':
      return { text: 'in progress', tone: 'info' };
    case 'blocked':
      return { text: 'blocked', tone: 'warn' };
    case 'completed':
      return { text: 'completed', tone: 'ok' };
    case 'pending':
      return { text: 'pending', tone: 'dim' };
    default:
      return { text: status ?? 'unknown', tone: 'plain' };
  }
}

/**
 * An instant as the reader's own wall clock, `YYYY-MM-DD HH:MM`.
 *
 * The reader's zone rather than the writer's: the stamp a record carries is
 * UTC, and a time only the machine that wrote it can place is not a time.
 */
function stamp(utc: string | null): string | null {
  if (utc === null) return null;
  const at = new Date(utc);
  if (Number.isNaN(at.getTime())) return null;
  const pad = (value: number): string => String(value).padStart(2, '0');
  return `${at.getFullYear()}-${pad(at.getMonth() + 1)}-${pad(at.getDate())} ${pad(
    at.getHours(),
  )}:${pad(at.getMinutes())}`;
}

/** A whole number off the wire, or null for anything else. */
function countOf(value: unknown): number | null {
  return typeof value === 'number' && Number.isInteger(value) && value >= 0 ? value : null;
}
