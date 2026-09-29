/**
 * Slack-style `:shortcode:` emoji: the set, and the order a query offers it in.
 *
 * **Static, and it does not cross the wire.** The table is the same for every
 * reader, so it ships in the bundle and the dropdown opens without a round
 * trip; what the server sends for a seat is its slash commands and its
 * subagent types, which are facts about that session rather than this one.
 *
 * The set is the typeahead's own in the terminal
 * (`crates/forge-tui/src/app/emoji_set.rs`): a curated ~200 GitHub / Slack
 * shortcodes rather than the full Unicode list, sorted by name. The ORDER is
 * the typeahead's decision and lives here with it, ported from
 * `crates/forge-tui/src/app/emoji.rs` - a table hands back a set, and a
 * dropdown that ranked with its own order would offer the right emoji in the
 * wrong order.
 */

/** One shortcode and the glyph it lands. */
export interface Emoji {
  name: string;
  glyph: string;
}

/**
 * Characters a query needs before it selects anything. One is too eager -
 * `:D` and a bare `: ` would both select mid-sentence.
 */
export const MIN_QUERY_CHARS = 2;

/** Whether `char` can appear in a shortcode: GitHub's own set is `[a-z0-9_+-]`. */
export function isShortcodeChar(char: string): boolean {
  return /^[a-z0-9_+-]$/.test(char);
}

/** Every shortcode forge knows, sorted by name. */
export const TABLE: Emoji[] = [
  { name: '+1', glyph: '\u{1F44D}' },
  { name: '-1', glyph: '\u{1F44E}' },
  { name: '100', glyph: '\u{1F4AF}' },
  { name: 'alarm_clock', glyph: '\u{23F0}' },
  { name: 'alien', glyph: '\u{1F47D}' },
  { name: 'anchor', glyph: '\u{2693}' },
  { name: 'angry', glyph: '\u{1F620}' },
  { name: 'art', glyph: '\u{1F3A8}' },
  { name: 'astonished', glyph: '\u{1F632}' },
  { name: 'avocado', glyph: '\u{1F951}' },
  { name: 'balloon', glyph: '\u{1F388}' },
  { name: 'banana', glyph: '\u{1F34C}' },
  { name: 'bar_chart', glyph: '\u{1F4CA}' },
  { name: 'battery', glyph: '\u{1F50B}' },
  { name: 'beer', glyph: '\u{1F37A}' },
  { name: 'bell', glyph: '\u{1F514}' },
  { name: 'birthday', glyph: '\u{1F382}' },
  { name: 'blush', glyph: '\u{1F60A}' },
  { name: 'bomb', glyph: '\u{1F4A3}' },
  { name: 'book', glyph: '\u{1F4D5}' },
  { name: 'bookmark', glyph: '\u{1F516}' },
  { name: 'books', glyph: '\u{1F4DA}' },
  { name: 'boom', glyph: '\u{1F4A5}' },
  { name: 'brain', glyph: '\u{1F9E0}' },
  { name: 'broom', glyph: '\u{1F9F9}' },
  { name: 'bug', glyph: '\u{1F41B}' },
  { name: 'building_construction', glyph: '\u{1F3D7}' },
  { name: 'bulb', glyph: '\u{1F4A1}' },
  { name: 'cake', glyph: '\u{1F370}' },
  { name: 'calendar', glyph: '\u{1F4C5}' },
  { name: 'camera', glyph: '\u{1F4F7}' },
  { name: 'candle', glyph: '\u{1F56F}' },
  { name: 'cat', glyph: '\u{1F431}' },
  { name: 'chart_with_downwards_trend', glyph: '\u{1F4C9}' },
  { name: 'chart_with_upwards_trend', glyph: '\u{1F4C8}' },
  { name: 'check', glyph: '\u{2714}' },
  { name: 'cherry_blossom', glyph: '\u{1F338}' },
  { name: 'clap', glyph: '\u{1F44F}' },
  { name: 'clipboard', glyph: '\u{1F4CB}' },
  { name: 'clock', glyph: '\u{1F550}' },
  { name: 'cloud', glyph: '\u{2601}' },
  { name: 'coffee', glyph: '\u{2615}' },
  { name: 'cold_sweat', glyph: '\u{1F630}' },
  { name: 'computer', glyph: '\u{1F4BB}' },
  { name: 'confetti_ball', glyph: '\u{1F38F}' },
  { name: 'confused', glyph: '\u{1F615}' },
  { name: 'construction', glyph: '\u{1F6A7}' },
  { name: 'cookie', glyph: '\u{1F36A}' },
  { name: 'cow', glyph: '\u{1F42E}' },
  { name: 'crab', glyph: '\u{1F980}' },
  { name: 'crossed_fingers', glyph: '\u{1F91E}' },
  { name: 'cry', glyph: '\u{1F622}' },
  { name: 'dancer', glyph: '\u{1F483}' },
  { name: 'dart', glyph: '\u{1F3AF}' },
  { name: 'dizzy', glyph: '\u{1F4AB}' },
  { name: 'dog', glyph: '\u{1F436}' },
  { name: 'door', glyph: '\u{1F6AA}' },
  { name: 'dragon', glyph: '\u{1F409}' },
  { name: 'drum', glyph: '\u{1F941}' },
  { name: 'ear', glyph: '\u{1F442}' },
  { name: 'earth_americas', glyph: '\u{1F30E}' },
  { name: 'eggplant', glyph: '\u{1F346}' },
  { name: 'envelope', glyph: '\u{2709}' },
  { name: 'exploding_head', glyph: '\u{1F92F}' },
  { name: 'eyes', glyph: '\u{1F440}' },
  { name: 'facepalm', glyph: '\u{1F926}' },
  { name: 'fire', glyph: '\u{1F525}' },
  { name: 'fireworks', glyph: '\u{1F386}' },
  { name: 'fish', glyph: '\u{1F41F}' },
  { name: 'floppy_disk', glyph: '\u{1F4BE}' },
  { name: 'flushed', glyph: '\u{1F633}' },
  { name: 'fox', glyph: '\u{1F98A}' },
  { name: 'frowning', glyph: '\u{1F641}' },
  { name: 'gear', glyph: '\u{2699}' },
  { name: 'gem', glyph: '\u{1F48E}' },
  { name: 'ghost', glyph: '\u{1F47B}' },
  { name: 'gift', glyph: '\u{1F381}' },
  { name: 'globe_with_meridians', glyph: '\u{1F310}' },
  { name: 'grimacing', glyph: '\u{1F62C}' },
  { name: 'grin', glyph: '\u{1F601}' },
  { name: 'hammer', glyph: '\u{1F528}' },
  { name: 'hammer_and_wrench', glyph: '\u{1F6E0}' },
  { name: 'handshake', glyph: '\u{1F91D}' },
  { name: 'hankey', glyph: '\u{1F4A9}' },
  { name: 'heart', glyph: '\u{2764}' },
  { name: 'heart_eyes', glyph: '\u{1F60D}' },
  { name: 'hearts', glyph: '\u{1F495}' },
  { name: 'hourglass', glyph: '\u{231B}' },
  { name: 'house', glyph: '\u{1F3E0}' },
  { name: 'hugs', glyph: '\u{1F917}' },
  { name: 'ice_cube', glyph: '\u{1F9CA}' },
  { name: 'inbox_tray', glyph: '\u{1F4E5}' },
  { name: 'information_source', glyph: '\u{2139}' },
  { name: 'joy', glyph: '\u{1F602}' },
  { name: 'key', glyph: '\u{1F511}' },
  { name: 'keyboard', glyph: '\u{2328}' },
  { name: 'label', glyph: '\u{1F3F7}' },
  { name: 'laptop', glyph: '\u{1F4BB}' },
  { name: 'leaves', glyph: '\u{1F343}' },
  { name: 'lemon', glyph: '\u{1F34B}' },
  { name: 'link', glyph: '\u{1F517}' },
  { name: 'lock', glyph: '\u{1F512}' },
  { name: 'loudspeaker', glyph: '\u{1F4E2}' },
  { name: 'mag', glyph: '\u{1F50D}' },
  { name: 'mailbox', glyph: '\u{1F4EB}' },
  { name: 'medal', glyph: '\u{1F3C5}' },
  { name: 'megaphone', glyph: '\u{1F4E3}' },
  { name: 'memo', glyph: '\u{1F4DD}' },
  { name: 'microscope', glyph: '\u{1F52C}' },
  { name: 'money_with_wings', glyph: '\u{1F4B8}' },
  { name: 'monkey', glyph: '\u{1F412}' },
  { name: 'moon', glyph: '\u{1F319}' },
  { name: 'mouse', glyph: '\u{1F42D}' },
  { name: 'muscle', glyph: '\u{1F4AA}' },
  { name: 'mushroom', glyph: '\u{1F344}' },
  { name: 'musical_note', glyph: '\u{1F3B5}' },
  { name: 'nail_care', glyph: '\u{1F485}' },
  { name: 'neutral_face', glyph: '\u{1F610}' },
  { name: 'no_entry', glyph: '\u{26D4}' },
  { name: 'notebook', glyph: '\u{1F4D3}' },
  { name: 'octopus', glyph: '\u{1F419}' },
  { name: 'ok_hand', glyph: '\u{1F44C}' },
  { name: 'open_file_folder', glyph: '\u{1F4C2}' },
  { name: 'outbox_tray', glyph: '\u{1F4E4}' },
  { name: 'owl', glyph: '\u{1F989}' },
  { name: 'package', glyph: '\u{1F4E6}' },
  { name: 'page_facing_up', glyph: '\u{1F4C4}' },
  { name: 'paperclip', glyph: '\u{1F4CE}' },
  { name: 'parrot', glyph: '\u{1F99C}' },
  { name: 'party_popper', glyph: '\u{1F389}' },
  { name: 'peach', glyph: '\u{1F351}' },
  { name: 'pencil', glyph: '\u{270F}' },
  { name: 'penguin', glyph: '\u{1F427}' },
  { name: 'phone', glyph: '\u{1F4DE}' },
  { name: 'pig', glyph: '\u{1F437}' },
  { name: 'pill', glyph: '\u{1F48A}' },
  { name: 'pizza', glyph: '\u{1F355}' },
  { name: 'point_down', glyph: '\u{1F447}' },
  { name: 'point_left', glyph: '\u{1F448}' },
  { name: 'point_right', glyph: '\u{1F449}' },
  { name: 'point_up', glyph: '\u{1F446}' },
  { name: 'poop', glyph: '\u{1F4A9}' },
  { name: 'pray', glyph: '\u{1F64F}' },
  { name: 'punch', glyph: '\u{1F44A}' },
  { name: 'pushpin', glyph: '\u{1F4CC}' },
  { name: 'question', glyph: '\u{2753}' },
  { name: 'rabbit', glyph: '\u{1F430}' },
  { name: 'rainbow', glyph: '\u{1F308}' },
  { name: 'raised_hands', glyph: '\u{1F64C}' },
  { name: 'recycle', glyph: '\u{267B}' },
  { name: 'robot', glyph: '\u{1F916}' },
  { name: 'rocket', glyph: '\u{1F680}' },
  { name: 'rofl', glyph: '\u{1F923}' },
  { name: 'rose', glyph: '\u{1F339}' },
  { name: 'sailboat', glyph: '\u{26F5}' },
  { name: 'salt', glyph: '\u{1F9C2}' },
  { name: 'satellite', glyph: '\u{1F4E1}' },
  { name: 'scissors', glyph: '\u{2702}' },
  { name: 'scroll', glyph: '\u{1F4DC}' },
  { name: 'seedling', glyph: '\u{1F331}' },
  { name: 'shark', glyph: '\u{1F988}' },
  { name: 'shield', glyph: '\u{1F6E1}' },
  { name: 'shipit', glyph: '\u{1F6A2}' },
  { name: 'shrug', glyph: '\u{1F937}' },
  { name: 'skull', glyph: '\u{1F480}' },
  { name: 'sleeping', glyph: '\u{1F634}' },
  { name: 'slightly_smiling_face', glyph: '\u{1F642}' },
  { name: 'smile', glyph: '\u{1F604}' },
  { name: 'smiley', glyph: '\u{1F603}' },
  { name: 'smirk', glyph: '\u{1F60F}' },
  { name: 'snail', glyph: '\u{1F40C}' },
  { name: 'snake', glyph: '\u{1F40D}' },
  { name: 'snowflake', glyph: '\u{2744}' },
  { name: 'sob', glyph: '\u{1F62D}' },
  { name: 'sos', glyph: '\u{1F198}' },
  { name: 'sparkles', glyph: '\u{2728}' },
  { name: 'speech_balloon', glyph: '\u{1F4AC}' },
  { name: 'star', glyph: '\u{2B50}' },
  { name: 'stopwatch', glyph: '\u{23F1}' },
  { name: 'sunglasses', glyph: '\u{1F60E}' },
  { name: 'sunny', glyph: '\u{2600}' },
  { name: 'sweat_smile', glyph: '\u{1F605}' },
  { name: 'tada', glyph: '\u{1F389}' },
  { name: 'telescope', glyph: '\u{1F52D}' },
  { name: 'test_tube', glyph: '\u{1F9EA}' },
  { name: 'thinking', glyph: '\u{1F914}' },
  { name: 'thread', glyph: '\u{1F9F5}' },
  { name: 'thumbsdown', glyph: '\u{1F44E}' },
  { name: 'thumbsup', glyph: '\u{1F44D}' },
  { name: 'toolbox', glyph: '\u{1F9F0}' },
  { name: 'trophy', glyph: '\u{1F3C6}' },
  { name: 'turtle', glyph: '\u{1F422}' },
  { name: 'unicorn', glyph: '\u{1F984}' },
  { name: 'unlock', glyph: '\u{1F513}' },
  { name: 'upside_down_face', glyph: '\u{1F643}' },
  { name: 'volcano', glyph: '\u{1F30B}' },
  { name: 'warning', glyph: '\u{26A0}' },
  { name: 'wastebasket', glyph: '\u{1F5D1}' },
  { name: 'wave', glyph: '\u{1F44B}' },
  { name: 'whale', glyph: '\u{1F433}' },
  { name: 'wheelchair', glyph: '\u{267F}' },
  { name: 'wink', glyph: '\u{1F609}' },
  { name: 'wolf', glyph: '\u{1F43A}' },
  { name: 'wrench', glyph: '\u{1F527}' },
  { name: 'x', glyph: '\u{274C}' },
  { name: 'yarn', glyph: '\u{1F9F6}' },
  { name: 'zany_face', glyph: '\u{1F92A}' },
  { name: 'zap', glyph: '\u{26A1}' },
  { name: 'zzz', glyph: '\u{1F4A4}' },
];

/**
 * The matches for `query`, best first: the exact shortcode, then the ones that
 * start with it, then the rest that contain it. Ties break alphabetically,
 * which the table's own ordering already provides.
 */
export function matches(query: string): Emoji[] {
  if (query.length < MIN_QUERY_CHARS) return [];
  const scored: { rank: number; emoji: Emoji }[] = [];
  for (const emoji of TABLE) {
    const rank = emoji.name === query ? 0 : emoji.name.startsWith(query) ? 1 : emoji.name.includes(query) ? 2 : -1;
    if (rank >= 0) scored.push({ rank, emoji });
  }
  scored.sort((a, b) => a.rank - b.rank || a.emoji.name.localeCompare(b.emoji.name));
  return scored.map((entry) => entry.emoji);
}

/**
 * The `:shortcode` the text ends in, or `null` when it ends in anything else.
 *
 * The `:` counts only at the start of the text or directly after whitespace, so
 * `http://`, `10:30` and `note:` open nothing, and everything between the `:`
 * and the end has to be shortcode characters - which a space is not, so a token
 * that is over closes it.
 */
export function shortcodeQuery(text: string): string | null {
  const chars = [...text];
  let at = chars.length;
  while (at > 0) {
    at -= 1;
    const char = chars[at];
    if (char === ':') {
      if (at > 0 && !/\s/.test(chars[at - 1] ?? '')) return null;
      return chars.slice(at + 1).join('');
    }
    if (char === undefined || !isShortcodeChar(char)) return null;
  }
  return null;
}
