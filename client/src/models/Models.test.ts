// @vitest-environment jsdom
import { readFileSync } from 'node:fs';

import { JSDOM } from 'jsdom';
import { flushSync, mount, tick, unmount } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

import { Microphone } from '../composer/mic';
import type {
  BenchResult,
  BenchTarget,
  BenchTier,
  CatalogueRow,
  DictateModelsWire,
  ModelRole,
  ReadAloudRecording,
} from '../wire/models';
import Models from './Models.svelte';
import ModelsBody from './ModelsBody.svelte';
import type { SetRecorder } from './recorder.svelte';
import { fakeConnection, MODELS, modelsWire } from './testing';
import type { SweepVerdict } from './view';

const drawn: ReturnType<typeof mount>[] = [];
const hosts: HTMLElement[] = [];

afterEach(() => {
  vi.restoreAllMocks();
  for (const component of drawn.splice(0)) void unmount(component);
  for (const host of hosts.splice(0)) host.remove();
});

/**
 * What a forge with `[dictate] enabled` unset answers: no pins, no check, and
 * no rows - the catalogue is only read while the section is on.
 */
function offWire(): DictateModelsWire {
  return {
    ...modelsWire,
    enabled: false,
    in_use: [],
    updates: [],
    check: { state: 'never' },
    rows: [],
  };
}

/** The page as it draws, mounted so a control can be reached. */
function open(
  wire: DictateModelsWire = modelsWire,
  handlers: Partial<{
    oncheck: () => void;
    oninstall: (variant: string) => void;
    onactivate: (file: string) => void;
    ondeactivate: (role: ModelRole) => void;
    onbench: (target: BenchTarget, tier: BenchTier) => void;
    onbenchstop: () => void;
    onrecord: () => void;
    onrecordstop: (keep: boolean) => void;
    onrecorddelete: (recording: ReadAloudRecording) => void;
    recorder: Pick<SetRecorder, 'wire'> | null;
    onbenchdelete: (result: BenchResult) => void;
    onupdate: (variant: string) => void;
    updated: { file: string; role: ModelRole } | null;
    refusal: string | null;
    onsweep: () => void;
    onsweepcancel: () => void;
    onadopt: (variant: string, role: ModelRole) => void;
    onuninstall: (file: string) => void;
    verdicts: SweepVerdict[];
  }> = {},
) {
  const host = document.createElement('div');
  document.body.append(host);
  hosts.push(host);
  const component = mount(ModelsBody, {
    target: host,
    props: {
      wire,
      oncheck: handlers.oncheck ?? (() => {}),
      oninstall: handlers.oninstall ?? (() => {}),
      onactivate: handlers.onactivate ?? (() => {}),
      ondeactivate: handlers.ondeactivate ?? (() => {}),
      onbench: handlers.onbench ?? (() => {}),
      onbenchstop: handlers.onbenchstop ?? (() => {}),
      onrecord: handlers.onrecord ?? (() => {}),
      onrecordstop: handlers.onrecordstop ?? (() => {}),
      onrecorddelete: handlers.onrecorddelete ?? (() => {}),
      recorder: handlers.recorder ?? null,
      onbenchdelete: handlers.onbenchdelete ?? (() => {}),
      onupdate: handlers.onupdate ?? (() => {}),
      updated: handlers.updated ?? null,
      refusal: handlers.refusal ?? null,
      onsweep: handlers.onsweep ?? (() => {}),
      onsweepcancel: handlers.onsweepcancel ?? (() => {}),
      onadopt: handlers.onadopt ?? (() => {}),
      onuninstall: handlers.onuninstall ?? (() => {}),
      verdicts: handlers.verdicts ?? [],
    },
  });
  drawn.push(component);
  return host;
}

describe('the models page as it draws', () => {
  /**
   * The two pins, with the facts each side is read from: the pin's own
   * declaration on the first line, the feed's measurement on the second, and
   * the live state as a chip.
   */
  it('draws the models in use', () => {
    const html = open().innerHTML;

    expect(html).toContain('cohere-transcribe-03-2026-Q4_K_M.gguf');
    expect(html).toContain('1.56 GB');
    expect(html).toContain('Q4_K_M');
    expect(html).toContain('sha 0ea56826');
    expect(html).toContain('72.9\u{d7} realtime on m4-max');
    expect(html).toContain('loaded');
    // The normalizer is in no feed and still draws: its own runtime stands
    // where the measurement would be.
    expect(html).toContain('s1-mini-f16.gguf');
    expect(html).toContain('llama.cpp');
    expect(html).toContain('not in the feed');
  });

  /**
   * The proposal's comparison, both sides of both numbers, and the link an
   * adoption takes: the note under it says a proposal becomes a pull request.
   */
  it('draws what the feed proposes', () => {
    const html = open().innerHTML;

    expect(html).toContain('update available');
    // The card that says there is an update shows the update: the pick, its
    // facts against the model in use, and why it is the pick.
    expect(html).toContain('Granite Speech 5.0 470M TurboCTC NC');
    expect(html).toContain('401.6\u{d7} vs 72.9\u{d7}');
    expect(html).toContain('FLEURS-en 4.3 vs 5.08');
    expect(html).toContain('Update to this model');
    // Normalised: the template wraps the sentence, and a line break inside
    // the phrase is not what this test is about.
    expect(html.replace(/\s+/g, ' ')).toContain(
      "beats the model in use on both of the feed's own measurements",
    );
    // The table says what it is read against, and carries every candidate
    // with the numbers the rule compared.
    expect(html).toContain('read against the transcribing model in use');
    // **The licence is a column, not a filter**: the non-commercial pick is
    // recommended and the row states what it would run under.
    expect(html).toContain('CC-BY-NC-SA-4.0');
    expect(html).toContain('recommended');
    // And the rows under it say why they lost.
    expect(html).toContain('also beats both, but slower');
    expect(html).toContain('slower than this');
  });

  /**
   * A check that is out is drawn as the core's own state, with no Check now
   * to press: a second check is refused, and a control that reports nothing
   * when pressed reads as broken.
   */
  it('draws a check in flight without the control to start another', () => {
    const host = open({
      ...modelsWire,
      check: { state: 'checking' },
    });

    expect(host.textContent).toContain('checking the catalogue');
    const check = [...host.querySelectorAll('button')].find((c) =>
      c.textContent?.includes('Check now'),
    );
    expect(check, 'a check in flight offers another').toBeUndefined();
  });

  /** The check's own click is what dispatches, once per press. */
  it('asks for a check when the control is pressed', () => {
    let checks = 0;
    const host = open(modelsWire, {
      oncheck: () => {
        checks += 1;
      },
    });

    const button = [...host.querySelectorAll<HTMLButtonElement>('button')].find((c) =>
      c.textContent?.includes('Check now'),
    );
    expect(button, 'the check control did not draw').not.toBeUndefined();
    button?.click();
    flushSync();

    expect(checks).toBe(1);
  });

  /**
   * **The box is blind on its own, so the page offers what can be searched
   * before a name is known**: the feed's families as chips and its fastest
   * rows as links, and both SET THE QUERY - a pick is the same mechanism as
   * typing, which is what makes a chip a way in rather than a label.
   */
  it('offers the families and the fastest rows, and a pick filters the list', () => {
    const host = open();
    const names = (): string[] =>
      [...host.querySelectorAll('.models .cand .nm')].map((el) => el.textContent ?? '');

    const link = host.querySelector<HTMLButtonElement>('.models .link');
    expect(link, 'no fastest row was offered').not.toBeNull();
    // The control carries its own number too: `name 401.6x`.
    const pick = (link?.textContent ?? '').trim().split(' ')[0] ?? '';
    expect(pick, 'the fastest link names nothing').not.toBe('');

    link?.click();
    flushSync();
    expect(names().length, 'a fastest link selected nothing').toBeGreaterThan(0);
    // Every row shown is a match for the picked name - the pick sets the
    // query, it is not a second filtering mechanism.
    for (const name of names()) {
      expect(name.toLowerCase()).toContain(pick.toLowerCase());
    }

    // And a family chip does the same for its class.
    const input = host.querySelector('input');
    if (input !== null) {
      input.value = '';
      input.dispatchEvent(new Event('input', { bubbles: true }));
      flushSync();
    }
    const chip = host.querySelector<HTMLButtonElement>('.models .chips .chip');
    expect(chip, 'the page offered nothing to browse').not.toBeNull();
    expect(chip?.textContent, 'the biggest family is not first').toContain('granite');
    chip?.click();
    flushSync();

    expect(names().length, 'a family chip selected nothing').toBeGreaterThan(0);
    for (const name of names()) {
      expect(name.toLowerCase()).toContain('granite');
    }
  });

  /**
   * Finding a model is this side's: the whole feed arrives with the read, so
   * the box filters what is here as it is typed and asks the server for
   * nothing. Each match is a LINK to the entry's own document - a row that
   * goes nowhere is what a reader clicks first and finds nothing behind.
   */
  it('filters the feed as the box is typed in, and each row opens its entry', () => {
    const host = open();
    const input = host.querySelector('input');
    expect(input).not.toBeNull();
    // Nothing typed: no list at all.
    expect(host.textContent).toContain('type a name');

    if (input !== null) {
      input.value = 'parakeet';
      input.dispatchEvent(new Event('input', { bubbles: true }));
      flushSync();
    }

    expect(host.textContent).toContain('parakeet-unified-en-0.6b');
    const results = host.querySelector('[aria-label="Catalogue results"]');
    expect(results?.textContent, 'a row nothing matched was drawn').not.toContain(
      'granite-speech-5.0-470m-turboctc',
    );
    expect(host.textContent).toContain('English only');
    expect(host.textContent).toContain('streaming');

    const row = host.querySelector<HTMLAnchorElement>('.cand');
    expect(row?.tagName, 'a result is not a link').toBe('A');
    expect(row?.getAttribute('href')).toContain('/catalog/parakeet-unified-en-0.6b.json');
    expect(row?.getAttribute('target')).toBe('_blank');
  });

  /**
   * **The hairline between rows survives the row becoming a link, and then a
   * control beside it.** A `:last-child` written on the ROW matches none of
   * them - the control follows the anchor - so the separator would die in the
   * whole list, which is what the anchor restructure did silently once
   * already. The rule belongs to the li.
   *
   * **Asserted on selectors rather than computed styles**, and deliberately:
   * jsdom drops a `var()` inside a shorthand (`border-bottom: 1px solid
   * var(--line)` computes to `0px none`), so a cascade read here would be an
   * instrument that cannot see the very rule it guards. What runs instead is
   * the real selector engine over the real markup.
   */
  it('keeps a hairline between candidate rows, and none under the last', () => {
    const host = open();
    const input = host.querySelector('input');
    if (input !== null) {
      input.value = 'granite';
      input.dispatchEvent(new Event('input', { bubbles: true }));
      flushSync();
    }

    const sheet = readFileSync('src/assets/web.css', 'utf8');
    const dom = new JSDOM(`<style>${sheet}</style><div class="models">${host.innerHTML}</div>`);
    // Only the CANDIDATE list's rows: the bench list draws `li`s too, and
    // they hold no anchor.
    const rows = [...dom.window.document.querySelectorAll('.models .list li')].filter(
      (li) => li.querySelector('.cand') !== null,
    );
    expect(rows.length, 'the search drew no rows to read').toBeGreaterThan(1);

    // The hazard itself, so a reader sees why the rule cannot key on the
    // row: each row's control follows its anchor, so no `.cand` is its li's
    // last child and a hairline rule written on the row would be dead in the
    // whole list.
    for (const li of rows) {
      expect(
        li.querySelector('.cand')?.matches('.models .cand:last-child'),
        "a row that IS its li's last child - the rule could key on the row",
      ).toBe(false);
    }
    // And the rules that decide it: the last li drops the hairline, and no
    // `.cand` rule claims one.
    expect(rows[0]?.matches('.models .list li:last-child'), 'the first row is the last').toBe(
      false,
    );
    expect(
      rows[rows.length - 1]?.matches('.models .list li:last-child'),
      'the last row does not carry the rule that drops its hairline',
    ).toBe(true);
    expect(sheet, 'the hairline rule is back on the row, which kills it list-wide').not.toContain(
      '.models .cand:last-child',
    );
    // Read as text, because the computed value is what jsdom cannot give.
    // **Both halves, and the second is the one that needs its own pin**:
    // `matches()` above never consults the stylesheet, and an ADD-rule regex
    // alone is satisfied by the `li` rule, so deleting the drop rule (or
    // flipping it to a selector that matches nothing) would ship a stray
    // hairline under the last row with every test green.
    expect(sheet, 'nothing draws the hairline on the li').toMatch(
      /\.models \.list li \{[^}]*border-bottom/,
    );
    expect(sheet, "nothing drops the last row's hairline").toMatch(
      /\.models \.list li:last-child \{[^}]*border-bottom: 0/,
    );
  });

  /**
   * **A catalogue that never landed is its own state.** With dictation on and
   * no rows - a first enable offline, or the boot fetch still out - the
   * discovery area drew "pick a family below" over ZERO chips, a pointer at
   * nothing beside an Updates line already saying the feed could not be
   * reached. The box and its helpers are for a feed that is here.
   */
  it('names an unread catalogue rather than offering nothing to browse', () => {
    const host = open({ ...modelsWire, rows: [] });

    expect(host.textContent).toContain('the catalogue has not been read yet');
    expect(host.querySelector('.models .chips'), 'chips drew with no rows behind them').toBeNull();
    expect(host.textContent, 'the page pointed at families that are not there').not.toContain(
      'pick a family below',
    );
  });

  it('says so when nothing matches', () => {
    const host = open();
    const input = host.querySelector('input');
    if (input !== null) {
      input.value = 'wav2vec';
      input.dispatchEvent(new Event('input', { bubbles: true }));
      flushSync();
    }

    expect(host.textContent).toContain('no entry matches');
    expect(host.querySelector('.cand')).toBeNull();
  });

  /**
   * Dictation off is its own state, and the page says which key is unset
   * rather than drawing an empty list that reads as a broken page.
   *
   * **The whole snapshot of an off forge, not part of it.** Nothing starts
   * the preflight or the catalogue read while the section is off, so the
   * server answers no rows and no check: a search box here would answer every
   * query with "no entry matches", and the page has to say why instead.
   */
  it('draws the off state, which has no catalogue to search', () => {
    const host = open(offWire());

    expect(host.textContent).toContain('dictation is off');
    expect(host.textContent).toContain('[dictate] enabled');
    expect(host.textContent).toContain('not checked yet');
    // No search box, and the note says why: the feed is not read.
    expect(host.querySelector('input'), 'an off forge draws a search box').toBeNull();
    expect(host.textContent).toContain('read only with');
    // And no Check now: with `[dictate]` off the core refuses the check, so a
    // control here would report nothing when pressed.
    expect(host.querySelector('.status button'), 'an off forge draws a Check now').toBeNull();
  });

  /**
   * A check state this client does not know says so, and never draws as a
   * fresh one: `up to date` here would be a claim about a feed nothing read.
   */
  it('draws a check state it cannot read as its own state', () => {
    const host = open({ ...modelsWire, check: { state: 'unknown' } as never, updates: [] });

    expect(host.textContent).toContain('this client');
    // The check line's own title: a state this client cannot read must not
    // draw as fresh. The roles' news below is a separate line, and its own
    // guard turns an unread feed into `the feed has not answered` rather
    // than a fresh claim.
    const title = host.querySelector('.status .t');
    expect(title?.textContent).not.toContain('up to date');
  });

  /** A feed that did not answer carries its own reason, in the error tone. */
  it('draws a check that could not reach the catalogue', () => {
    const host = open({
      ...modelsWire,
      check: { state: 'unreachable', error: 'github.com answered 502' },
      updates: [],
    });

    expect(host.textContent).toContain('the catalogue could not be reached');
    expect(host.textContent).toContain('github.com answered 502');
    // **An empty `updates` on a feed that never answered is not "nothing to
    // propose"**: the roles' news reads the same check state the title does.
    expect(
      host.textContent,
      'a role read as up to date against a feed that never answered',
    ).toContain('the feed has not answered');
  });

  /**
   * A failure a reader has closed stays closed - and a NEW failure draws
   * rather than hiding behind a dismissal that was about something else.
   */
  it('lets a failed bench line be closed, and draws the next one', () => {
    const failed = (file: string, reason: string): DictateModelsWire => ({
      ...modelsWire,
      bench: { state: 'failed', target: { file, role: 'transcribing', pinned: false }, reason },
    });

    const host = open(failed('a-norm-a.gguf', 'No such file or directory (os error 2)'));
    expect(host.textContent).toContain('the bench did not finish');

    const close = [...host.querySelectorAll<HTMLButtonElement>('button')].find(
      (c) => c.textContent === 'close',
    );
    expect(close, 'the failed line drew no way to close it').not.toBeUndefined();
    close?.click();
    flushSync();

    expect(host.textContent, 'the closed line kept drawing').not.toContain(
      'the bench did not finish',
    );

    // Another model failing is its own line, not the closed one.
    const again = open(failed('b-norm-b.gguf', 'the file is not a model'));
    expect(again.textContent).toContain('the bench did not finish');
  });

  /**
   * A model this machine downloaded and nothing runs carries the control
   * that removes it - what a sweep left behind, or a download the runtime no
   * longer needs. One in use does not: the core refuses it by name.
   */
  it('offers remove on an installed model that nothing runs', () => {
    const removed: string[] = [];
    const host = open(modelsWire, { onuninstall: (file) => removed.push(file) });

    const remove = [...host.querySelectorAll<HTMLButtonElement>('button')].filter(
      (c) => c.textContent === 'remove',
    );
    // Exactly the two downloaded models nothing runs: the two in use are not
    // offered, because the core refuses to remove what runs.
    expect(remove.length, 'the removable rows are the installed ones').toBe(2);
    remove[0]?.click();
    flushSync();

    expect(removed).toEqual(['granite-speech-5.0-470m-turboctc-nc-Q4_K_M.gguf']);
  });

  /**
   * The line that says there is an update shows the update: the pick's own
   * row carries the model, its numbers against what runs, and the one
   * control - and the table under it keeps none, because a second control
   * for the same press reads as a second action.
   */
  it('shows the recommendation with one control, and none in the table', () => {
    const host = open(modelsWire);

    expect(host.textContent).toContain('Granite Speech 5.0 470M TurboCTC NC');
    expect(host.textContent).toContain('replaces the transcribing model');
    const controls = [...host.querySelectorAll<HTMLButtonElement>('button')].filter((c) =>
      c.textContent?.includes('Update to this model'),
    );
    expect(controls.length, 'the update control is drawn more than once').toBe(1);
  });

  /**
   * The recommended candidate's own control, which is what makes the UPDATES
   * line actionable rather than a statement. The fixture's candidate is
   * already installed here, so the control is the activation and it names the
   * file the core should load.
   */
  it('offers the recommended candidate as the transcribing model', () => {
    const updates: string[] = [];
    const host = open(modelsWire, { onupdate: (variant) => updates.push(variant) });

    const button = [...host.querySelectorAll<HTMLButtonElement>('button')].find((c) =>
      c.textContent?.includes('Update to this model'),
    );
    expect(button, 'the recommended row drew no update control').not.toBeUndefined();
    button?.click();
    flushSync();

    expect(updates).toEqual(['granite-speech-5.0-470m-turboctc-nc']);
  });

  /**
   * A candidate this machine does not have offers the download instead, and
   * the press names the variant - the feed's own verb, which is what the core
   * resolves a doc and a URL from.
   */
  it('offers the download for a candidate this machine does not have', () => {
    const updates: string[] = [];
    const host = open(
      { ...modelsWire, installed: [] },
      { onupdate: (variant) => updates.push(variant) },
    );

    // The one control is the update itself: it downloads first, then loads.
    const button = [...host.querySelectorAll<HTMLButtonElement>('button')].find((c) =>
      c.textContent?.includes('Update to this model'),
    );
    expect(button, 'the recommended row drew no update control').not.toBeUndefined();
    button?.click();
    flushSync();

    expect(updates).toEqual(['granite-speech-5.0-470m-turboctc-nc']);
  });

  /**
   * The download as it runs: the core's own file name, the whole percent and
   * the bytes, with the progress element carrying the same figure.
   */
  it('draws a download in flight with its percent', () => {
    const host = open({
      ...modelsWire,
      install: {
        state: 'downloading',
        file: 'granite-speech-5.0-470m-turboctc-Q4_K_M.gguf',
        got: 100_000_000,
        total: 200_000_000,
      },
    });

    expect(host.textContent).toContain('downloading granite-speech-5.0-470m-turboctc-Q4_K_M.gguf');
    expect(host.textContent).toContain('50%');
    expect(host.textContent).toContain('of 200 MB');
    const bar = host.querySelector<HTMLProgressElement>('progress');
    expect(bar?.value, 'the bar and the words must be the same figure').toBe(50);
  });

  /**
   * **A failed download says why, in the core's words, and nothing calls a
   * downloaded file verified.** The note says what is actually checked: the
   * entry's byte length for every file, a digest for the ones whose source
   * publishes one - the page must not upgrade that into a claim nobody can
   * back.
   */
  it("draws a failed download in the core's words without claiming a verification", () => {
    const host = open({
      ...modelsWire,
      install: {
        state: 'failed',
        file: 'granite-speech-5.0-470m-turboctc-Q4_K_M.gguf',
        reason: 'granite-speech-5.0-470m-turboctc-Q4_K_M.gguf is 5 bytes, expected 6',
      },
    });

    expect(host.textContent).toContain('the download did not finish');
    expect(host.textContent).toContain('is 5 bytes, expected 6');
    expect(host.textContent?.toLowerCase()).not.toContain('verified');
    // Both halves of the rule, because the cleanup feed's files DO publish
    // one and a note that said otherwise would be false about them.
    expect(host.textContent).toContain('publish none');
    expect(host.textContent).toContain('sha256');
  });

  /**
   * Where the model in use came from, in the row's own words: a config pin
   * names the key that has to go for the runtime to move the role, and the
   * role offers no activation control while it stands.
   */
  it('names the [dictate] key that pins a role, and offers no activation for it', () => {
    const host = open({
      ...modelsWire,
      // Nothing installed here, so the download control is the one the pinned
      // role must keep.
      installed: [],
      in_use: modelsWire.in_use.map((model) =>
        model.role === 'transcribing'
          ? {
              ...model,
              from: {
                from: 'config',
                key: 'transcribe_model',
                variant: 'cohere-transcribe-03-2026',
              },
            }
          : model,
      ),
    });

    expect(host.textContent).toContain('pinned by [dictate] transcribe_model');
    const activation = [...host.querySelectorAll<HTMLButtonElement>('button')].find((c) =>
      c.textContent?.includes('Update to this model'),
    );
    expect(activation, 'a pinned role drew the update control').toBeUndefined();
    // The download stays: a pinned role can still pull candidates down.
    const download = [...host.querySelectorAll<HTMLButtonElement>('button')].find((c) =>
      c.textContent?.includes('install'),
    );
    expect(download, 'a pinned role lost its download control').not.toBeUndefined();
  });

  /**
   * The benchmark section: what can be scored, the marks that say what each
   * row IS, and the control that starts a run.
   */
  it('lists what can be benched, marks it, and dispatches a run', () => {
    const runs: { target: BenchTarget; tier: BenchTier }[] = [];
    const running = {
      ...modelsWire,
      bench: {
        state: 'running',
        target: {
          file: 'cohere-transcribe-03-2026-Q4_K_M.gguf',
          role: 'transcribing',
          pinned: false,
        },
        tier: 'consensus',
        clip: 4,
        clips: 15,
        so_far: 0.5,
      },
    } as DictateModelsWire;
    const host = open(running, { onbench: (target, tier) => runs.push({ target, tier }) });

    expect(host.textContent).toContain('benching cohere-transcribe-03-2026-Q4_K_M.gguf');
    expect(host.textContent).toContain('clip 4 of 15');
    expect(host.textContent).toContain('50% agreed so far');

    const stop = [...host.querySelectorAll('button')].find((c) =>
      c.textContent?.includes('stop the bench'),
    );
    expect(stop, 'the running bench offers no stop').not.toBeUndefined();

    // The list says what each row is: the model in use, the feed's pick.
    const rows = [...host.querySelectorAll('.models .list li')].filter(
      (li) => li.querySelector('.bench-row') !== null,
    );
    const marked = rows.map((li) => li.textContent ?? '');
    expect(marked.find((text) => text.includes('cohere-transcribe-03-2026-Q4_K_M.gguf'))).toContain(
      'in use',
    );
    expect(
      marked.find((text) => text.includes('granite-speech-5.0-470m-turboctc-nc-Q4_K_M.gguf')),
    ).toContain('recommended');

    const benchButtons = [...host.querySelectorAll<HTMLButtonElement>('button')].filter((c) =>
      c.textContent?.includes('bench it'),
    );
    expect(benchButtons.length).toBeGreaterThan(0);
    benchButtons[0]?.click();
    flushSync();
    expect(runs.map((run) => run.tier)).toEqual(['consensus']);
  });

  /** What a finished run measured, with its corpus named. */
  it('draws a saved bench result with its own numbers', () => {
    const host = open({
      ...modelsWire,
      results: [
        {
          target: {
            file: 'cohere-transcribe-03-2026-Q4_K_M.gguf',
            role: 'transcribing',
            pinned: false,
          },
          tier: 'consensus',
          metrics: {
            clips: 15,
            audio_seconds: 156.7,
            wall_seconds: 3.4,
            xrt_wall: 45.7,
            term_accuracy: null,
            wer: 0.036,
            matched: [9, 15],
            stages_ms: {
              model_load_ms: 398,
              resample_ms: 12,
              mel_ms: 4,
              encode_ms: 1470,
              decode_ms: 664,
              normalize_ms: 1006,
            },
          },
          at: '2026-10-06T19:11:38Z',
          corpus: { clips: 15, audio_seconds: 156, sha256: 'a414db2a' },
        },
      ],
    });

    expect(host.textContent).toContain('45.7\u{d7} realtime');
    expect(host.textContent).toContain('WER 3.6%');
    expect(host.textContent).toContain('9 of 15 matched a baseline');
    expect(host.textContent).toContain('your own takes');
    expect(host.textContent).toContain('15 clips');
    expect(host.textContent).toContain('encode 1470ms');
    expect(host.textContent).toContain('this is the model in use');

    // A result for ANOTHER model says what it would take to compare: a run of
    // the model in use over the same corpus - never a comparison invented
    // across two different corpora.
    const other = open({
      ...modelsWire,
      results: [
        {
          target: {
            file: 'granite-speech-5.0-470m-turboctc-Q4_K_M.gguf',
            role: 'transcribing',
            pinned: false,
          },
          tier: 'consensus',
          metrics: {
            clips: 30,
            audio_seconds: 313.4,
            wall_seconds: 7.2,
            xrt_wall: 43.3,
            term_accuracy: null,
            wer: 0.147,
            matched: [3, 30],
            stages_ms: {
              model_load_ms: 122,
              resample_ms: 9,
              mel_ms: 3,
              encode_ms: 2011,
              decode_ms: 900,
              normalize_ms: 1200,
            },
          },
          at: '2026-10-07T01:20:38Z',
          corpus: { clips: 30, audio_seconds: 313, sha256: 'bbbb' },
        },
      ],
    });
    expect(other.textContent).toContain(
      'no run of cohere-transcribe-03-2026-Q4_K_M.gguf over this same corpus to compare with yet',
    );
  });

  /**
   * **A role row is the selector**: the proposal the Updates section draws is
   * the pressed role's, and a role the feed has nothing for says so rather
   * than leaving the last role's table standing under another role's name.
   */
  it('draws the pressed role and says when one has no proposal', () => {
    const host = open();
    const roles = [...host.querySelectorAll<HTMLButtonElement>('button.pick')];
    const word = (text: string) =>
      roles.find((button) => button.getAttribute('aria-label')?.includes(text) === true);

    const transcribing = word('transcribing');
    const cleanup = word('cleanup');
    expect(transcribing, 'the in-use rows are the selector').not.toBeUndefined();
    expect(cleanup).not.toBeUndefined();
    expect(transcribing?.getAttribute('aria-pressed'), 'the first proposal is drawn').toBe('true');
    expect(host.textContent).toContain('read against the transcribing model in use');

    cleanup?.click();
    flushSync();
    expect(host.textContent, 'the cleanup role draws its own view').toContain(
      'the bench decides this role',
    );
    expect(host.textContent).not.toContain('read against the transcribing model in use');

    transcribing?.click();
    flushSync();
    expect(host.textContent).toContain('read against the transcribing model in use');
    expect(host.textContent).not.toContain('the bench decides this role');
  });

  /**
   * **The cleanup role's candidates are the Hub's rows, and the bench decides
   * them.** Each candidate carries this machine's own runs where it has any,
   * the control installs or loads it into the cleanup slot, and the pick
   * names the best run only when two of them share a corpus - one run is a
   * number, not a comparison.
   */
  it('draws the cleanup candidates, their runs, and the pick the bench makes', () => {
    const cleanupRow: CatalogueRow = {
      variant: 'mradermacher/CeluneNorm-0.6B-v2.0-ctx2048-GGUF',
      display_name: 'mradermacher/CeluneNorm-0.6B-v2.0-ctx2048-GGUF',
      family: 'mradermacher',
      params: 600_000_000,
      license: 'MIT',
      languages: ['en'],
      streaming: false,
      speed: null,
      wer: null,
      download: { quant: 'Q4_K_M', size_bytes: 396_000_000 },
      kind: 'normalizer',
      url: 'https://huggingface.co/mradermacher/CeluneNorm-0.6B-v2.0-ctx2048-GGUF',
      download_count: 449,
    };
    const record = {
      variant: cleanupRow.variant,
      file: 'CeluneNorm-0.6B-v2.0-ctx2048-Q4_K_M.gguf',
      url: 'https://huggingface.co/mradermacher/CeluneNorm-0.6B-v2.0-ctx2048-GGUF/resolve/main/x.gguf',
      size: 396_000_000,
      facts: { quant: 'Q4_K_M', params: 600_000_000, license: 'MIT', runtime: 'llama.cpp' },
      at: '2026-10-07T02:00:00Z',
    };
    const run = (wer: number, speed: number) => ({
      target: { file: record.file, role: 'cleanup' as const, pinned: false },
      tier: 'consensus' as const,
      metrics: {
        clips: 10,
        audio_seconds: 250,
        wall_seconds: 4,
        xrt_wall: speed,
        term_accuracy: null,
        wer,
        matched: [5, 10] as [number, number],
        stages_ms: {
          model_load_ms: 100,
          resample_ms: 1,
          mel_ms: 1,
          encode_ms: 1,
          decode_ms: 1,
          normalize_ms: 1,
        },
      },
      at: '2026-10-07T03:00:00Z',
      corpus: { clips: 10, audio_seconds: 250, sha256: 'a414db2a' },
    });
    const host = open({
      ...modelsWire,
      rows: [cleanupRow],
      installed: [record],
      results: [run(0.04, 60)],
    });
    const cleanup = [...host.querySelectorAll<HTMLButtonElement>('button.pick')].find((button) =>
      button.getAttribute('aria-label')?.includes('cleanup'),
    );
    cleanup?.click();
    flushSync();

    expect(host.textContent).toContain('mradermacher/CeluneNorm-0.6B-v2.0-ctx2048-GGUF');
    expect(host.textContent).toContain('396 MB');
    expect(host.textContent, 'the candidate is installable or loadable').toContain(
      'use for cleanup',
    );
    expect(host.textContent, 'its own run is on its row').toContain('WER 4.0%');
    expect(host.textContent, 'one run is a number, not a comparison').toContain(
      'nothing here is benched twice over one corpus with words known to be true',
    );
  });

  /** The read-aloud set: not recorded draws the passage and the record press. */
  it('offers the read-aloud set once and its passage', () => {
    let records = 0;
    const host = open(modelsWire, { onrecord: () => (records += 1) });

    expect(host.textContent).toContain('the read-aloud set is not recorded yet');
    expect(host.textContent).toContain('I want the forge session to pick up where it left off.');
    const record = [...host.querySelectorAll<HTMLButtonElement>('button')].find((c) =>
      c.textContent?.includes('record the passage'),
    );
    expect(record, 'the record control did not draw').not.toBeUndefined();
    record?.click();
    flushSync();
    expect(records).toBe(1);
  });

  /**
   * **A recording draws what it is and how to end it, both ways.** The card
   * is the page's own state while the microphone is open - stop and save
   * writes the set, cancel throws the audio away - and a failure the core
   * could not answer is drawn here.
   */
  it('draws the recording with its two ways out', () => {
    const stops: boolean[] = [];
    const host = open(
      { ...modelsWire, read_aloud: { ...modelsWire.read_aloud, recording: true } },
      {
        onrecordstop: (keep) => stops.push(keep),
        recorder: { wire: { frames: 40, bytes: 2_560, rate: 51_200, dbfs: [], elapsedMs: 800 } },
      },
    );

    expect(host.textContent).toContain('recording the passage');
    expect(host.textContent).toContain('I want the forge session to pick up where it left off.');
    expect(host.textContent).not.toContain('the read-aloud set is not recorded yet');
    const buttons = [...host.querySelectorAll<HTMLButtonElement>('button')];
    const save = buttons.find((c) => c.textContent?.includes('stop and save'));
    const cancel = buttons.find((c) => c.textContent?.includes('cancel'));
    expect(save, 'a recording must offer to save').not.toBeUndefined();
    expect(cancel, 'a recording must offer to cancel').not.toBeUndefined();
    save?.click();
    cancel?.click();
    flushSync();
    expect(stops).toEqual([true, false]);
  });

  /**
   * **Recorded is a list, not a prompt.** Once a recording stands, the page
   * shows what is there - its length and when it was made - with a delete
   * per row and a way to add another, and the passage is not drawn again: a
   * reader who has read it does not need it under every state.
   */
  it('lists the recordings, each with its own delete and a way to add one', () => {
    const deleted: ReadAloudRecording[] = [];
    const host = open(
      {
        ...modelsWire,
        read_aloud: {
          ...modelsWire.read_aloud,
          recordings: [
            {
              id: 'take-1791363000000',
              duration_ms: 31_400,
              bytes: 1_004_800,
              sha256: '8f9a2c41deadbeef',
              at: '2026-10-07T09:30:00Z',
            },
            {
              id: 'take-1791363600000',
              duration_ms: 18_000,
              bytes: 576_000,
              sha256: '11aa22bb33cc44dd',
              at: '2026-10-07T09:40:00Z',
            },
          ],
        },
      },
      { onrecorddelete: (recording) => deleted.push(recording) },
    );

    expect(host.textContent).toContain('the read-aloud set');
    expect(host.textContent).not.toContain('the read-aloud set is not recorded yet');
    expect(host.textContent).not.toContain('I want the forge session to pick up where it left off');
    expect(host.textContent, 'the lengths are the rows').toContain('0:31');
    expect(host.textContent).toContain('0:18');
    expect(host.textContent).toContain('recorded');
    expect(host.textContent, 'a row carries more than its length').toContain('1 MB');
    expect(host.textContent, 'and what identifies those samples').toContain('sha 8f9a2c41');
    expect(host.textContent, 'a list is not a prompt').not.toContain('record the passage');

    const deletes = [...host.querySelectorAll<HTMLButtonElement>('button')].filter((button) =>
      button.textContent?.includes('delete'),
    );
    expect(deletes).toHaveLength(2);
    deletes[0]?.click();
    flushSync();
    expect(deleted.map((recording) => recording.id)).toEqual(['take-1791363000000']);

    const another = [...host.querySelectorAll<HTMLButtonElement>('button')].find((button) =>
      button.textContent?.includes('record another'),
    );
    expect(another, 'a recorded set must offer one more').not.toBeUndefined();
  });

  /** A write that failed after the stop is drawn in the core's own words. */
  it('draws the read-aloud write failure the read carried', () => {
    const host = open({
      ...modelsWire,
      read_aloud: { ...modelsWire.read_aloud, error: 'the set directory is not writable' },
    });

    expect(host.textContent).toContain('the set directory is not writable');
  });

  /** A refused action is drawn in the core's own words, at the page's top. */
  it('draws a refused action in the words the core sent', () => {
    const host = open(modelsWire, {
      refusal:
        '[dictate] transcribe_model pins this model in forge.toml; remove the key to change it here',
    });

    expect(host.textContent).toContain('[dictate] transcribe_model pins this model in forge.toml');
    expect(host.querySelector('.status.failed[role="alert"]')).not.toBeNull();
  });
});

describe('the models route as it draws', () => {
  /** No connection, no read: the page says it is reading rather than drawing
   * an empty catalogue. */
  it('draws the loading line before the first read lands', () => {
    const host = document.createElement('div');
    document.body.append(host);
    hosts.push(host);
    const component = mount(Models, {
      target: host,
      props: { connection: null },
    });
    drawn.push(component);

    expect(host.textContent).toContain('Reading the models...');
    expect(host.querySelector('.models .block')).toBeNull();
  });

  /** Mount the route on a connection a test drives, over the given host. */
  function route(forge: ReturnType<typeof fakeConnection>): HTMLElement {
    const host = document.createElement('div');
    document.body.append(host);
    hosts.push(host);
    const component = mount(Models, { target: host, props: { connection: forge.connection } });
    drawn.push(component);
    return host;
  }

  /** Let the page's async seams - `Microphone.open`, a chained press - settle
   * before what they set off is read. A timer turn drains every microtask. */
  async function settle(): Promise<void> {
    await new Promise((resolve) => setTimeout(resolve, 0));
  }

  /**
   * **The page's live wiring, which no string-level test can see.** The route
   * subscribes the catalogue itself, hands the snapshot to the body, and its
   * Check now is the one caller of the command - three links a rename or a
   * deleted effect breaks silently: nothing would be subscribed, the page
   * would read nothing, and the press would do nothing, with every other test
   * green.
   */
  it('subscribes the catalogue, draws what it answers, and checks on its control', async () => {
    const forge = fakeConnection();
    const host = route(forge);
    await tick();

    expect(forge.subscribed, 'the route did not subscribe the catalogue').toEqual([MODELS]);

    forge.arrive({ kind: 'snapshot', subject: MODELS, data: modelsWire });
    await tick();
    expect(host.textContent, 'the snapshot never reached the page').toContain('update available');

    const check = host.querySelector<HTMLButtonElement>('.status button');
    expect(check, 'the check line drew no control to press').not.toBeNull();
    check?.click();
    flushSync();
    expect(forge.dispatched, 'Check now dispatched nothing').toEqual(['dictate_catalogue_check']);
    expect(forge.refreshed, 'the click did not ask for the read').toEqual([MODELS]);
  });

  /**
   * **Leaving the page releases the catalogue.** The effect's teardown is the
   * only thing that does it, and the route's own doc promises it: without it
   * the unsubscribe never goes, and a forge is left encoding a read nobody
   * draws for the rest of the session.
   */
  it('releases the catalogue when the route is left', async () => {
    const forge = fakeConnection();
    route(forge);
    await tick();
    expect(forge.subscribed).toEqual([MODELS]);

    // The route this test just mounted is the last one drawn.
    const component = drawn.pop();
    if (component === undefined) throw new Error('the route was not mounted');
    void unmount(component);

    expect(forge.unsubscribed, 'leaving the page left the catalogue subscribed').toEqual([MODELS]);
    expect(forge.listening(), 'the release left a listener on the connection').toBe(0);
  });

  /**
   * **Leaving the page releases the microphone.** The socket survives a
   * client-side route change, so a recording left running holds the input
   * with no page drawing it. The teardown stops it unkept - the audio of an
   * abandoned page is not saved as the set.
   */
  it('releases the microphone when the route is left mid-recording', async () => {
    const forge = fakeConnection();
    let micStopped = false;
    const mic = {
      onFrame: null as ((bytes: Uint8Array) => void) | null,
      flush: () => null,
      stop: () => {
        micStopped = true;
      },
    };
    vi.spyOn(Microphone, 'open').mockResolvedValue(mic as unknown as Microphone);
    const host = route(forge);
    await tick();
    forge.arrive({ kind: 'snapshot', subject: MODELS, data: modelsWire });
    await tick();

    const record = [...host.querySelectorAll<HTMLButtonElement>('button')].find(
      (c) => c.textContent === 'record the passage',
    );
    expect(record, 'the read-aloud card drew no way to record').not.toBeUndefined();
    record?.click();
    // `begin` opens the microphone and constructs the recorder across
    // microtasks: wait for them rather than counting ticks.
    await settle();

    // The route this test just mounted is the last one drawn.
    const component = drawn.pop();
    if (component === undefined) throw new Error('the route was not mounted');
    void unmount(component);

    expect(forge.dispatched, 'leaving mid-recording did not stop it, unkept').toContainEqual({
      dictate_read_aloud_stop: { keep: false },
    });
    expect(micStopped, 'leaving mid-recording left the microphone held').toBe(true);
  });

  /**
   * A refused subscription is the server's own words on the page. Without the
   * branch, the store's refusal never turns into a drawing and the page reads
   * `Reading the models...` for the life of the connection.
   */
  it('draws the refusal in the server words', async () => {
    const forge = fakeConnection();
    const host = route(forge);
    await tick();

    forge.arrive({ kind: 'error', what: 'subscribe', why: 'no models on this forge' });
    await tick();

    expect(host.textContent).toContain('no models on this forge');
    expect(host.textContent, 'a refusal drew the loading line').not.toContain('Reading the models');
  });

  /**
   * **The update row's control is the route's own action.** The body names
   * the file; the route writes the command. A break between them is a control
   * that draws and does nothing, with every drawing test still green.
   */
  it('dispatches the activation its row presses, and asks for the read', async () => {
    const forge = fakeConnection();
    const host = route(forge);
    await tick();
    forge.arrive({ kind: 'snapshot', subject: MODELS, data: modelsWire });
    await tick();

    // The fixture has the recommended model installed, so the press is the
    // swap itself - the route's chained update action.
    const button = [...host.querySelectorAll<HTMLButtonElement>('button')].find((c) =>
      c.textContent?.includes('Update to this model'),
    );
    button?.click();
    flushSync();

    expect(forge.dispatched).toEqual([
      {
        dictate_activate: {
          role: 'transcribing',
          file: 'granite-speech-5.0-470m-turboctc-nc-Q4_K_M.gguf',
        },
      },
    ]);
    expect(forge.refreshed, 'the press did not ask for the re-read').toEqual([MODELS]);
  });

  /**
   * **One press, a whole update.** The page chains the two core actions it
   * already has - download when the variant is not here, then load it - and
   * says so when the role runs it. Each step is answered by the wire's own
   * state, never by a timer.
   */
  it('updates to a recommended model: install, swap, and a completion line', async () => {
    const forge = fakeConnection();
    const host = route(forge);
    await tick();
    forge.arrive({ kind: 'snapshot', subject: MODELS, data: { ...modelsWire, installed: [] } });
    await tick();

    const button = [...host.querySelectorAll<HTMLButtonElement>('button')].find((c) =>
      c.textContent?.includes('Update to this model'),
    );
    expect(button, 'the update control did not draw').not.toBeUndefined();
    button?.click();
    flushSync();
    expect(forge.dispatched).toEqual([
      { dictate_install: { variant: 'granite-speech-5.0-470m-turboctc-nc' } },
    ]);

    // The download lands: the record arrives, and the page loads it.
    const record = {
      variant: 'granite-speech-5.0-470m-turboctc-nc',
      file: 'granite-speech-5.0-470m-turboctc-nc-Q4_K_M.gguf',
      url: 'https://huggingface.co/handy-computer/granite-speech-5.0-470m-turboctc-nc-gguf/resolve/main/x.gguf',
      size: 279_000_000,
      facts: { quant: 'Q4_K_M', params: 473_014_752, license: 'CC-BY-NC-SA-4.0', runtime: null },
      at: '2026-10-07T02:00:00Z',
    };
    forge.arrive({
      kind: 'snapshot',
      subject: MODELS,
      data: { ...modelsWire, installed: [record] },
    });
    await tick();
    expect(forge.dispatched[1]).toEqual({
      dictate_activate: { role: 'transcribing', file: record.file },
    });

    // The swap lands: the role runs it, and the page says the update is done.
    forge.arrive({
      kind: 'snapshot',
      subject: MODELS,
      data: {
        ...modelsWire,
        installed: [record],
        in_use: modelsWire.in_use.map((model) =>
          model.role === 'transcribing' ? { ...model, file: record.file } : model,
        ),
      },
    });
    await tick();
    expect(host.textContent).toContain('update completed');
    expect(host.textContent).toContain('granite-speech-5.0-470m-turboctc-nc-Q4_K_M.gguf is now');
  });

  /**
   * A refused dispatch arrives as an `error` frame, and the page draws the
   * core's words. Without the listener a press on a pinned role's row would
   * look like nothing happened - which is precisely what that refusal is.
   */
  it("draws a refused action in the core's words", async () => {
    const forge = fakeConnection();
    const host = route(forge);
    await tick();
    forge.arrive({ kind: 'snapshot', subject: MODELS, data: modelsWire });
    await tick();

    forge.arrive({
      kind: 'error',
      what: 'dispatch',
      why: '[dictate] transcribe_model pins this model in forge.toml; remove the key to change it here',
    });
    await tick();

    expect(host.textContent).toContain('[dictate] transcribe_model pins this model in forge.toml');
  });

  /** One cleanup candidate as the feed lists it: popularity and a size. */
  function normRow(variant: string, downloads: number, size = 300_000_000): CatalogueRow {
    return {
      variant,
      display_name: variant,
      family: 'norm',
      params: 600_000_000,
      license: null,
      languages: ['en'],
      streaming: false,
      download: { quant: 'Q4_K_M', size_bytes: size },
      speed: null,
      wer: null,
      kind: 'normalizer',
      url: `https://huggingface.co/${variant}`,
      download_count: downloads,
    };
  }

  /** The record a landed install writes for one candidate. */
  function recordFor(variant: string, file: string, size = 300_000_000) {
    return {
      variant,
      file,
      url: `https://huggingface.co/${variant}`,
      size,
      facts: { quant: 'Q4_K_M', params: 600_000_000, license: null, runtime: 'llama.cpp' },
      at: '2026-10-07T09:00:00Z',
    };
  }

  /**
   * Stopping the sweep stops its run too. A cancel that only cleared the card
   * would leave the core loading and scoring a model for a press nobody is
   * waiting on any more.
   */
  it('stops the bench the sweep started when the sweep is stopped', async () => {
    const forge = fakeConnection();
    const host = route(forge);
    await tick();
    const withRow = { ...modelsWire, rows: [normRow('a/norm-a', 900)] };
    forge.arrive({ kind: 'snapshot', subject: MODELS, data: withRow });
    await tick();

    const button = [...host.querySelectorAll<HTMLButtonElement>('button')].find(
      (c) => c.textContent === 'run the benchmark',
    );
    button?.click();
    flushSync();
    await tick();

    forge.arrive({
      kind: 'snapshot',
      subject: MODELS,
      data: {
        ...withRow,
        installed: [...modelsWire.installed, recordFor('a/norm-a', 'a-norm-a-Q4_K_M.gguf')],
      },
    });
    await tick();
    forge.arrive({
      kind: 'snapshot',
      subject: MODELS,
      data: {
        ...withRow,
        installed: [...modelsWire.installed, recordFor('a/norm-a', 'a-norm-a-Q4_K_M.gguf')],
        bench: {
          state: 'running',
          target: {
            file: 'granite-speech-5.0-470m-turboctc-nc-Q4_K_M.gguf',
            role: 'transcribing',
            pinned: false,
          },
          tier: 'consensus',
          clip: 1,
          clips: 3,
          so_far: null,
        },
      },
    });
    await tick();

    const stop = [...host.querySelectorAll<HTMLButtonElement>('button')].find(
      (c) => c.textContent === 'stop the sweep',
    );
    expect(stop, 'the sweep stop did not draw').not.toBeUndefined();
    stop?.click();
    flushSync();

    expect(forge.dispatched.at(-1)).toBe('dictate_bench_stop');
  });

  /**
   * A failure that was already standing does not stop a sweep. The core keeps
   * a failed bench until the next one, and the next one is this sweep's: a
   * chain that treated the settled failure as its own would download
   * candidate bytes and then die on a bench that finished yesterday.
   */
  it('sweeps over a failure that was already standing', async () => {
    const forge = fakeConnection();
    const host = route(forge);
    await tick();
    forge.arrive({
      kind: 'snapshot',
      subject: MODELS,
      data: {
        ...modelsWire,
        rows: [],
        bench: {
          state: 'failed',
          target: { file: 'a/norm-old.gguf', role: 'cleanup', pinned: false },
          reason: 'No such file or directory (os error 2)',
        },
      },
    });
    await tick();

    const button = [...host.querySelectorAll<HTMLButtonElement>('button')].find(
      (c) => c.textContent === 'run the benchmark',
    );
    button?.click();
    flushSync();
    await tick();

    // The plan's first run is benched: the stale failure is not the sweep's.
    expect(forge.dispatched.at(-1)).toEqual({
      dictate_bench: {
        target: {
          file: 'granite-speech-5.0-470m-turboctc-nc-Q4_K_M.gguf',
          role: 'transcribing',
          pinned: false,
        },
        tier: 'consensus',
      },
    });
  });

  /**
   * **A failure standing on a plan file does not feed itself.** The core
   * keeps a failed bench until the next one, and a deterministic failure -
   * the file's bytes are gone - fails identically when the sweep benches it
   * again: reading that as the failure that was already standing re-dispatches
   * on every push, forever. One dispatch, then the sweep ends by name.
   */
  it('benches a standing failure once, then ends', async () => {
    const forge = fakeConnection();
    const host = route(forge);
    await tick();
    const file = 'granite-speech-5.0-470m-turboctc-nc-Q4_K_M.gguf';
    const failing = {
      state: 'failed' as const,
      target: { file, role: 'transcribing' as const, pinned: false },
      reason: 'No such file or directory (os error 2)',
    };
    forge.arrive({
      kind: 'snapshot',
      subject: MODELS,
      data: { ...modelsWire, rows: [], bench: failing },
    });
    await tick();

    const button = [...host.querySelectorAll<HTMLButtonElement>('button')].find(
      (c) => c.textContent === 'run the benchmark',
    );
    button?.click();
    flushSync();
    await tick();

    // The sweep's own bench of that file goes out - once.
    expect(forge.dispatched.at(-1)).toEqual({
      dictate_bench: {
        target: { file, role: 'transcribing', pinned: false },
        tier: 'consensus',
      },
    });
    const dispatched = forge.dispatched.length;

    // It fails the same way. That is this sweep's answer, not the standing
    // one, and the sweep ends rather than dispatching again.
    forge.arrive({
      kind: 'snapshot',
      subject: MODELS,
      data: { ...modelsWire, rows: [], bench: failing },
    });
    await tick();

    expect(forge.dispatched.length, 'a repeated failure must not re-bench').toBe(dispatched);
    expect(host.textContent, 'the sweep ended on the failure').toContain(
      'No such file or directory',
    );
    expect(host.textContent, 'the card is back for the next press').toContain('run the benchmark');
  });

  /**
   * The same shape one arm over: a standing INSTALL failure - on a variant
   * this sweep never asked for - must not end it before it has asked for
   * anything.
   */
  it('sweeps over a standing install failure it never asked for', async () => {
    const forge = fakeConnection();
    const host = route(forge);
    await tick();
    forge.arrive({
      kind: 'snapshot',
      subject: MODELS,
      data: {
        ...modelsWire,
        rows: [normRow('a/norm-a', 900)],
        install: {
          state: 'failed',
          file: 'a/norm-old.gguf',
          reason: 'No such file or directory (os error 2)',
        },
      },
    });
    await tick();

    const button = [...host.querySelectorAll<HTMLButtonElement>('button')].find(
      (c) => c.textContent === 'run the benchmark',
    );
    button?.click();
    flushSync();
    await tick();

    // The plan's own fetch goes out: the stale failure is not this sweep's.
    expect(forge.dispatched.at(-1)).toEqual({
      dictate_install: { variant: 'a/norm-a' },
    });
  });

  /**
   * **A take landing mid-sweep moves the corpus under the runs**, and the
   * core recomputes it per run: the chain would bench every run again on
   * every push, forever. The move ends the sweep by name instead.
   */
  it('ends the sweep by name when a take moves the corpus under it', async () => {
    const forge = fakeConnection();
    const host = route(forge);
    await tick();
    forge.arrive({ kind: 'snapshot', subject: MODELS, data: { ...modelsWire, rows: [] } });
    await tick();

    const button = [...host.querySelectorAll<HTMLButtonElement>('button')].find(
      (c) => c.textContent === 'run the benchmark',
    );
    button?.click();
    flushSync();
    await tick();

    // The first run lands on one corpus, and the next is benched.
    forge.arrive({
      kind: 'snapshot',
      subject: MODELS,
      data: {
        ...modelsWire,
        rows: [],
        results: [
          benchResult(
            'granite-speech-5.0-470m-turboctc-nc-Q4_K_M.gguf',
            'transcribing',
            0.1,
            'first',
          ),
        ],
      },
    });
    await tick();
    expect(forge.dispatched.at(-1)).toMatchObject({
      dictate_bench: {
        target: { file: 'cohere-transcribe-03-2026-Q4_K_M.gguf', role: 'transcribing' },
      },
    });

    // A take lands: the next run's result is on another corpus.
    const before = forge.dispatched.length;
    forge.arrive({
      kind: 'snapshot',
      subject: MODELS,
      data: {
        ...modelsWire,
        rows: [],
        results: [
          benchResult('cohere-transcribe-03-2026-Q4_K_M.gguf', 'transcribing', 0.1, 'second'),
          benchResult(
            'granite-speech-5.0-470m-turboctc-nc-Q4_K_M.gguf',
            'transcribing',
            0.1,
            'first',
          ),
        ],
      },
    });
    await tick();

    expect(host.textContent).toContain('a take landed while the sweep ran');
    expect(forge.dispatched.length, 'a sweep that cannot score must not keep benching').toBe(
      before,
    );
  });

  /**
   * The check line names both roles, and names them without a press: the
   * feed proposes for the transcribing role and the bench decides the
   * cleanup one, so a reader hears there is news before pressing anything.
   */
  it('names each role on the check line, with no press needed', () => {
    const host = open(modelsWire);

    expect(host.textContent).toContain('transcribing has an update');
    expect(host.textContent).toContain('cleanup has no pick yet');

    const clear = open({ ...modelsWire, updates: [] });
    expect(clear.textContent).toContain('transcribing is up to date');
  });

  /**
   * The bench's cleanup pick carries the control that takes it: a pick with
   * no way to act on it is news a reader cannot use.
   */
  it("offers the switch on the bench's cleanup pick", () => {
    const adopted: [string, ModelRole][] = [];
    const host = open(
      {
        ...modelsWire,
        rows: [normRow('a/norm-a', 900), normRow('a/norm-b', 500)],
        installed: [
          ...modelsWire.installed,
          recordFor('a/norm-a', 'a-norm-a-Q4_K_M.gguf'),
          recordFor('a/norm-b', 'a-norm-b-Q4_K_M.gguf'),
        ],
        results: [
          benchResult('a-norm-a-Q4_K_M.gguf', 'cleanup', 0.07, 'gold', 'read_aloud'),
          benchResult('a-norm-b-Q4_K_M.gguf', 'cleanup', 0.12, 'gold', 'read_aloud'),
        ],
      },
      { onadopt: (variant, role) => adopted.push([variant, role]) },
    );

    // The cleanup role's view is the selected one, as the in-use row presses it.
    const pick = [...host.querySelectorAll<HTMLButtonElement>('button.pick')].find((c) =>
      (c.getAttribute('aria-label') ?? '').includes('cleanup'),
    );
    expect(pick, 'the cleanup selector did not draw').not.toBeUndefined();
    pick?.click();
    flushSync();

    expect(host.textContent).toContain('a/norm-a');
    expect(host.textContent).toContain('measured best on the read-aloud');
    // The run line names the corpus it was scored on: two runs under one
    // candidate are otherwise the same numbers about nothing in particular.
    expect(host.querySelector('li.run')?.textContent).toContain('the read-aloud passage');

    const button = [...host.querySelectorAll<HTMLButtonElement>('button')].find(
      (c) => c.textContent === 'switch to it',
    );
    expect(button, 'the pick drew no control').not.toBeUndefined();
    button?.click();
    flushSync();

    expect(adopted).toEqual([['a/norm-a', 'normalization']]);

    // The pick that IS what runs offers no switch: the control would be one
    // that changes nothing.
    const settled = open({
      ...modelsWire,
      rows: [normRow('a/norm-a', 900), normRow('a/norm-b', 500)],
      installed: [
        ...modelsWire.installed,
        recordFor('a/norm-a', 's1-mini-f16.gguf'),
        recordFor('a/norm-b', 'a-norm-b-Q4_K_M.gguf'),
      ],
      results: [
        benchResult('s1-mini-f16.gguf', 'cleanup', 0.07, 'gold', 'read_aloud'),
        benchResult('a-norm-b-Q4_K_M.gguf', 'cleanup', 0.12, 'gold', 'read_aloud'),
      ],
    });
    const settledPick = [...settled.querySelectorAll<HTMLButtonElement>('button.pick')].find((c) =>
      (c.getAttribute('aria-label') ?? '').includes('cleanup'),
    );
    settledPick?.click();
    flushSync();

    expect(
      [...settled.querySelectorAll<HTMLButtonElement>('button')].some(
        (c) => c.textContent === 'switch to it',
      ),
      'a switch onto the model already running was drawn',
    ).toBe(false);
  });

  /**
   * An old result does not define the sweep's corpus. The takes can have
   * moved since it was measured, so its corpus is not today's - and a sweep
   * that anchored on it would bench every run, score none of them against
   * it, and bench them again. The sweep's own first run names the corpus.
   */
  it('anchors the corpus on its own first run, not a result that was already here', async () => {
    const forge = fakeConnection();
    const host = route(forge);
    await tick();
    const stale = benchResult(
      'granite-speech-5.0-470m-turboctc-nc-Q4_K_M.gguf',
      'transcribing',
      0.2,
      'old',
    );
    stale.at = '2026-10-06T08:00:00Z';
    forge.arrive({ kind: 'snapshot', subject: MODELS, data: { ...modelsWire, results: [stale] } });
    await tick();

    const button = [...host.querySelectorAll<HTMLButtonElement>('button')].find(
      (c) => c.textContent === 'run the benchmark',
    );
    button?.click();
    flushSync();
    await tick();

    // The pick is benched first even though an old result names it: nothing
    // counts until a run lands after the press.
    expect(forge.dispatched.at(-1)).toEqual({
      dictate_bench: {
        target: {
          file: 'granite-speech-5.0-470m-turboctc-nc-Q4_K_M.gguf',
          role: 'transcribing',
          pinned: false,
        },
        tier: 'consensus',
      },
    });

    forge.arrive({
      kind: 'snapshot',
      subject: MODELS,
      data: {
        ...modelsWire,
        results: [
          benchResult(
            'granite-speech-5.0-470m-turboctc-nc-Q4_K_M.gguf',
            'transcribing',
            0.09,
            'now',
          ),
          stale,
        ],
      },
    });
    await tick();

    // Its corpus is the sweep's from here, and the next run follows.
    expect(forge.dispatched.at(-1)).toEqual({
      dictate_bench: {
        target: {
          file: 'cohere-transcribe-03-2026-Q4_K_M.gguf',
          role: 'transcribing',
          pinned: false,
        },
        tier: 'consensus',
      },
    });
  });

  /**
   * One finished run, in the shape its tier actually produces: a take-scored
   * run carries agreement and no error figure, a read-aloud run the other way
   * round. Fixtures that set an error figure on a take-scored run pin a shape
   * production cannot make.
   */
  function benchResult(
    file: string,
    role: 'transcribing' | 'cleanup',
    figure: number,
    corpus = 'new',
    tier: BenchTier = 'consensus',
  ): BenchResult {
    return {
      target: { file, role, pinned: false },
      tier,
      metrics: {
        clips: 12,
        audio_seconds: 320,
        wall_seconds: 210,
        xrt_wall: 30,
        term_accuracy: null,
        wer: tier === 'read_aloud' ? figure : null,
        matched: tier === 'consensus' ? [Math.round((1 - figure) * 12), 12] : null,
        stages_ms: {
          model_load_ms: 1200,
          resample_ms: 10,
          mel_ms: 20,
          encode_ms: 30,
          decode_ms: 40,
          normalize_ms: 50,
        },
      },
      at: '2026-10-07T10:00:00Z',
      corpus: { clips: 12, audio_seconds: 320, sha256: corpus },
    };
  }

  /**
   * The sweep's card prices the press before it spends - which runs, which
   * corpus, what has to come down - and the press is one dispatch.
   */
  it('prices the sweep before the press', () => {
    let presses = 0;
    const host = open(
      { ...modelsWire, rows: [normRow('a/norm-a', 900), normRow('a/norm-b', 500)] },
      { onsweep: () => (presses += 1) },
    );

    expect(host.textContent).toContain(
      "the feed's transcribing pick + the 2 most-downloaded cleanup candidates",
    );
    expect(host.textContent).toContain('5 runs over your takes');
    expect(host.textContent).toContain('about 600 MB to download');

    const button = [...host.querySelectorAll<HTMLButtonElement>('button')].find(
      (c) => c.textContent === 'run the benchmark',
    );
    expect(button, 'the sweep control did not draw').not.toBeUndefined();
    button?.click();
    flushSync();

    expect(presses).toBe(1);
  });

  /**
   * The verdict names what it saw and what to do: the scope carries the
   * candidate count, and the switch takes the winner into the role it read
   * best in - the cleanup one here, not transcribing's.
   */
  it('draws a verdict with its scope, and switches the role it read in', () => {
    const adopted: [string, ModelRole][] = [];
    const host = open(modelsWire, {
      onadopt: (variant, role) => adopted.push([variant, role]),
      verdicts: [
        {
          role: 'cleanup',
          best: {
            run: {
              variant: 'a/norm-a',
              role: 'cleanup',
              file: 'a-norm-a-Q4_K_M.gguf',
              size_bytes: 300_000_000,
              installed: false,
              why: 'candidate',
            },
            result: benchResult('a-norm-a-Q4_K_M.gguf', 'cleanup', 0.08),
          },
          baseline: benchResult('s1-mini-f16.gguf', 'cleanup', 0.12),
          onBest: false,
          scored: 4,
          beyond: 6,
          tried: 3,
          pick: false,
          tier: 'consensus',
        },
      ],
    });

    expect(host.textContent).toContain(
      'a-norm-a-Q4_K_M.gguf agreed with the baselines more often than the cleanup model',
    );
    expect(host.textContent).toContain(
      'best of the 3 most-downloaded cleanup candidates, scored on your takes, 12 clips',
    );
    expect(host.textContent).toContain('6 more candidates were not tried');
    expect(host.textContent).toContain('scored on your takes, where a run reads as agreement');

    const button = [...host.querySelectorAll<HTMLButtonElement>('button')].find(
      (c) => c.textContent === 'switch to it',
    );
    expect(button, 'the switch control did not draw').not.toBeUndefined();
    button?.click();
    flushSync();

    expect(adopted).toEqual([['a/norm-a', 'normalization']]);
  });

  /**
   * **What a switch costs is not always known.** A verdict's winner the feed
   * no longer carries is neither on this machine nor on a row, and the line
   * says so rather than reading as free.
   */
  it('says when the switch cost is not known', () => {
    const host = open(modelsWire, {
      verdicts: [
        {
          role: 'cleanup',
          best: {
            run: {
              variant: 'a/norm-gone',
              role: 'cleanup',
              file: 'a-norm-gone-Q4_K_M.gguf',
              size_bytes: 300_000_000,
              installed: false,
              why: 'candidate',
            },
            result: benchResult('a-norm-gone-Q4_K_M.gguf', 'cleanup', 0.08),
          },
          baseline: benchResult('s1-mini-f16.gguf', 'cleanup', 0.12),
          onBest: false,
          scored: 4,
          beyond: 0,
          tried: 1,
          pick: false,
          tier: 'consensus',
        },
      ],
    });

    expect(host.textContent).toContain('what switching costs is not known here');
  });

  /**
   * One press, and the sweep chains itself over the core's own pushes:
   * install the candidate that is not here, score every run on one corpus,
   * and hand the verdict back when the last one lands.
   */
  it("runs the sweep end to end on the core's own pushes", async () => {
    const forge = fakeConnection();
    const host = route(forge);
    await tick();
    forge.arrive({
      kind: 'snapshot',
      subject: MODELS,
      data: { ...modelsWire, rows: [normRow('a/norm-a', 900)] },
    });
    await tick();

    const button = [...host.querySelectorAll<HTMLButtonElement>('button')].find(
      (c) => c.textContent === 'run the benchmark',
    );
    expect(button, 'the sweep control did not draw').not.toBeUndefined();
    button?.click();
    flushSync();
    await tick();

    const withRow = { ...modelsWire, rows: [normRow('a/norm-a', 900)] };

    // The feeds are read fresh, and the one run that is not here comes down.
    expect(forge.dispatched).toEqual([
      'dictate_catalogue_check',
      { dictate_install: { variant: 'a/norm-a' } },
    ]);

    // A push lands mid-download with no record yet: the sweep waits on the
    // download rather than asking for it a second time.
    forge.arrive({ kind: 'snapshot', subject: MODELS, data: withRow });
    await tick();
    expect(forge.dispatched.filter((command) => typeof command !== 'string')).toEqual([
      { dictate_install: { variant: 'a/norm-a' } },
    ]);

    const record = {
      variant: 'a/norm-a',
      file: 'a-norm-a-Q4_K_M.gguf',
      url: 'https://huggingface.co/a/norm-a',
      size: 300_000_000,
      facts: { quant: 'Q4_K_M', params: 600_000_000, license: null, runtime: 'llama.cpp' },
      at: '2026-10-07T09:00:00Z',
    };
    forge.arrive({
      kind: 'snapshot',
      subject: MODELS,
      data: { ...withRow, installed: [...modelsWire.installed, record] },
    });
    await tick();

    // Then the runs, one at a time: each waits for the one before it.
    const runs = [
      { file: 'granite-speech-5.0-470m-turboctc-nc-Q4_K_M.gguf', role: 'transcribing' },
      { file: 'cohere-transcribe-03-2026-Q4_K_M.gguf', role: 'transcribing' },
      { file: 's1-mini-f16.gguf', role: 'cleanup' },
      { file: 'a-norm-a-Q4_K_M.gguf', role: 'cleanup' },
    ] as const;
    const results: BenchResult[] = [];
    for (const [at, run] of runs.entries()) {
      expect(forge.dispatched.at(-1)).toEqual({
        dictate_bench: {
          target: { file: run.file, role: run.role, pinned: false },
          tier: 'consensus',
        },
      });
      // The run lands, and the next one starts off the read it pushes.
      results.unshift(benchResult(run.file, run.role, 0.1 + at / 100, 'sweep'));
      forge.arrive({
        kind: 'snapshot',
        subject: MODELS,
        data: {
          ...withRow,
          installed: [...modelsWire.installed, record],
          results,
        },
      });
      await tick();
    }

    // The verdict is in and the file nobody adopted goes back off the disk.
    expect(forge.dispatched.at(-1)).toEqual({
      dictate_uninstall: { file: 'a-norm-a-Q4_K_M.gguf' },
    });
    expect(host.textContent).toContain('agreed most of the');
    expect(host.textContent).toContain('most-downloaded cleanup candidate');

    // **The verdict is a record of that sweep.** A later read - another run
    // on another corpus - does not rewrite what it says it saw: the scope it
    // carries is the sweep's own set, not whatever the page holds now.
    const recorded = 'agreed most of the 2 scored';
    expect(host.textContent).toContain(recorded);

    forge.arrive({
      kind: 'snapshot',
      subject: MODELS,
      data: {
        ...withRow,
        installed: [...modelsWire.installed, record],
        results: [
          benchResult(
            'granite-speech-5.0-470m-turboctc-nc-Q4_K_M.gguf',
            'transcribing',
            0.02,
            'later',
          ),
        ],
      },
    });
    await tick();

    expect(host.textContent).toContain(recorded);
  });
});
