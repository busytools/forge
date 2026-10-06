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
import { firstText, obj, parsedText, str } from './result-json';
import { isSlackId } from './text';

/** One word on a forge row. The tone colours the word; the word carries it. */
export interface ForgeChip {
  text: string;
  tone: 'plain' | 'info' | 'ok' | 'warn' | 'bad' | 'dim';
}

/** One entry of a card's body list. */
export interface ForgeListItem {
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
export interface ForgeComment {
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
  | { kind: 'empty'; text: string };

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
  tail?: { text: string; tone: 'warn' | 'bad' } | null;
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

/** The class subscriptions a list answer carries: the DM class, the mentions target. */
function classSubscriptions(value: unknown): string[] {
  if (!Array.isArray(value)) return [];
  const out: string[] = [];
  for (const held of value) {
    const target = str(obj(held), 'target');
    if (target !== null && target.trim() !== '') out.push(target);
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
  active_form: string | null;
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
  // result only confirms that the write went through.
  if (family === 'slack' && (verbOf(name) === 'edit' || verbOf(name) === 'react')) {
    return firstText(result.content) === null ? null : acknowledgementCard(verbOf(name), input);
  }
  const answer = parsedText(result.content);
  if (family === 'tasks') return tasksCard(verbOf(name), input, answer);
  if (family === 'cron') return cronCard(verbOf(name), answer);
  if (family === 'gotify') return gotifyCard(verbOf(name), answer);
  if (family === 'slack') return slackCard(verbOf(name), input, answer);
  if (family === 'review') return reviewCard(verbOf(name), input, answer);
  if (family === 'agents') return agentsCard(verbOf(name), input, answer);
  return null;
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
  gotify: 'notifications',
  slack: 'conversations',
  review: 'reviews',
  agents: 'workers',
};

/**
 * What a forge call's row is titled by when it carries no card.
 *
 * Two cases have no card: a call still out, whose result has not landed, and
 * a failed one, whose result IS the reason and draws as every failure does.
 * Either way the title comes from the call's own input where that names a
 * subject - in the SUCCESS card's own wording, so a failed write reads
 * `spawned worker 'reviewer'` under a tail that says it failed - and from the
 * family's own noun where the input names nothing. Never the tool:
 * `forge: slack__list` is the plumbing a reader scans past.
 */
export function forgeRowTitle(
  name: string,
  input: unknown,
  outcome: 'running' | 'failed' = 'running',
): string | null {
  const family = forgeFamilyOf(name);
  if (family === null) return null;
  const verb = verbOf(name);
  const held = obj(input);
  if (outcome === 'failed') {
    const written = acknowledgementCard(verb, input);
    if (written !== null) return written.title;
    if (verb === 'post') return postTitle(str(held, 'workspace'), str(held, 'conversation'));
    const spawned = verb === 'spawn' ? str(held, 'label') : null;
    if (spawned !== null && spawned.trim() !== '') return `spawned worker '${spawned}'`;
  } else {
    const pending = pendingCard(family, verb, input);
    if (pending !== null) return pending.title;
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
 * A Slack write waits on the dock, and that wait is the one thing the row
 * must not hide; a spawn says what it is bringing up. Every other verb has
 * nothing to draw until it answers and falls back to its raw text.
 */
function pendingCard(family: ForgeFamily, verb: string, input: unknown): ForgeCard | null {
  const held = obj(input);
  // What the call says it is doing, where its input states it: a create or an
  // update names the state it is writing, and the row says it while it waits.
  const chips: ForgeChip[] = [];
  const asked = str(held, 'status');
  if (asked !== null && asked.trim() !== '') chips.push(statusChip(asked));
  if (verb === 'spawn') {
    const label = str(held, 'label');
    if (label === null) return null;
    const pieces: ForgePiece[] = [];
    const charter = str(held, 'charter');
    if (charter !== null && charter.trim() !== '') {
      pieces.push({ kind: 'tag', text: 'charter' });
      pieces.push({ kind: 'quote', text: charter });
    }
    return { title: `spawning ${label}`, chips, figure: null, pieces };
  }
  if (verb === 'post') {
    // The input carries an id and never a name, so the title spells it as
    // given: a `#` on an id would dress it as a channel.
    const channel = str(held, 'conversation');
    if (channel === null) return null;
    const said = str(held, 'text');
    return {
      title: postTitle(str(held, 'workspace'), channel, 'posting'),
      chips: [{ text: 'waiting for your approval', tone: 'warn' }],
      figure: null,
      pieces: said === null || said.trim() === '' ? [] : [{ kind: 'quote', text: said }],
    };
  }
  if (verb === 'edit') {
    const channel = str(held, 'conversation');
    if (channel === null) return null;
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
    const counts: Record<string, number> = {};
    for (const held of comments) {
      const status = str(obj(held), 'status') ?? 'unknown';
      counts[status] = (counts[status] ?? 0) + 1;
    }
    for (const status of ['open', 'addressed', 'outdated', 'resolved']) {
      const count = counts[status] ?? 0;
      if (count > 0) chips.push({ text: `${count} ${status}`, tone: statusTone(status) });
    }
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
      figure: comments.length === 0 ? null : `${comments.length} comments`,
      pieces,
    };
  }
  return null;
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
      pieces: items.length === 0 ? [] : [{ kind: 'list', items }],
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
    const workspace = str(obj(input), 'workspace');
    const where = name === null ? null : channelName(name);
    return {
      title: postTitle(workspace, where),
      chips: parts === null || parts <= 1 ? [] : [{ text: `${parts} parts`, tone: 'plain' }],
      figure: ts.length === 0 ? null : `ts ${ts[0]}`,
      pieces: [],
    };
  }
  if (verb === 'search') {
    if (!Array.isArray(answer)) return null;
    const items: ForgeListItem[] = [];
    for (const held of answer) {
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
      title: withWorkspace(what, str(obj(input), 'workspace')),
      chips: [],
      figure: items.length === 0 ? null : `${items.length} hits`,
      pieces: listBody(items, 'no messages matched the search'),
    };
  }
  if (verb === 'list') {
    const conversations = obj(answer)['conversations'];
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
    const classes = classSubscriptions(obj(answer)['subscriptions']);
    if (classes.length > 0) {
      pieces.push({ kind: 'kv', pairs: [['also watching', classes.join(', ')]] });
    }
    return {
      title: withWorkspace('conversations', str(obj(input), 'workspace')),
      chips: [],
      figure: items.length === 0 ? null : `${items.length}`,
      pieces,
    };
  }
  if (verb === 'pins' || verb === 'bookmarks') {
    if (!Array.isArray(answer)) return null;
    const items: ForgeListItem[] = [];
    for (const held of answer) {
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
      title: withWorkspace(title, str(obj(input), 'workspace')),
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

/** A read's title, naming the workspace the call went into when it named one. */
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
export function verbOf(name: string): string {
  const parts = name.split('__');
  return parts.length === 0 ? '' : (parts[parts.length - 1] ?? '');
}

/** The tasks family's card: create / update echo a record, delete an envelope, list an array. */
function tasksCard(verb: string, input: unknown, answer: unknown): ForgeCard | null {
  if (verb === 'create' || verb === 'update') {
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
        ...namedBy(input, verb === 'update'),
        ...recordFacts(record, verb === 'update' ? 'updated_at' : 'created_at'),
      ],
    });
    return {
      title: record.subject,
      chips: [statusChip(record.status)],
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
        when: record.owner ?? record.estimate,
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
  return names.length === 0 ? [] : [['changed', names.map(spaced).join(', ')]];
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
    active_form: str(held, 'active_form'),
    created_at: str(held, 'created_at'),
    updated_at: str(held, 'updated_at'),
  };
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

/** A field name as a row spells it: `active_form` is "active form". */
function spaced(name: string): string {
  return name.replaceAll('_', ' ');
}

/** A whole number off the wire, or null for anything else. */
function countOf(value: unknown): number | null {
  return typeof value === 'number' && Number.isInteger(value) && value >= 0 ? value : null;
}
