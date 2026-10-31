// @vitest-environment jsdom
/**
 * The push-to-talk key over a live takeover: the page under the bar owns the
 * keyboard, so a press of the binding must not start a take behind it.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { surface } from '../browser/surface.svelte';
import {
  type Dictation,
  pushToTalkHandlers,
  type PushToTalk,
  routeModifier,
} from './dictate.svelte';

/** The bound key on this platform: jsdom reports no mac. */
const BOUND = { code: 'ControlRight', key: 'Control' };

function wiring(
  over: Partial<PushToTalk> = {},
  recording = false,
  opening = false,
): {
  wiring: PushToTalk;
  act: ReturnType<typeof vi.fn>;
  began: ReturnType<typeof vi.fn>;
} {
  const act = vi.fn();
  const began = vi.fn();
  const dictate = { held: recording, opening, openingHere: opening, act } as unknown as Dictation;
  return {
    act,
    began,
    wiring: {
      dictate: () => dictate,
      offered: () => true,
      bind: () => 'right_cmd',
      mode: () => 'auto',
      machine: () => false,
      barLine: () => false,
      onBegin: began,
      ...over,
    },
  };
}

function press(handlers: ReturnType<typeof pushToTalkHandlers>): void {
  handlers.down(new KeyboardEvent('keydown', { code: BOUND.code, key: BOUND.key }));
}

function release(handlers: ReturnType<typeof pushToTalkHandlers>): void {
  handlers.up(new KeyboardEvent('keyup', { code: BOUND.code, key: BOUND.key }));
}

/** A box holding the caret, as a page draws one: the caret is read off the
 *  box's own editor name. */
function focusBox(editor: string): void {
  const box = document.createElement('textarea');
  box.dataset['editor'] = editor;
  document.body.append(box);
  box.focus();
}

beforeEach(() => {
  surface.phase = 'closed';
});

afterEach(() => {
  document.body.innerHTML = '';
});

describe('the push-to-talk key', () => {
  /// **A live takeover owns the keyboard.** A press while the browser surface
  /// is up must not begin a take behind the page the person is driving.
  it('starts no take while a browser takeover is up', () => {
    const { wiring: w, act, began } = wiring();
    const handlers = pushToTalkHandlers(w);
    surface.phase = 'live';
    press(handlers);
    expect(act, 'a press under a live takeover acts on nothing').not.toHaveBeenCalled();
    expect(began, 'and the keyboard is not moved to the composer').not.toHaveBeenCalled();
  });

  /// **A box standing down for a machine turn takes no keys.** The field is
  /// drawn as a line there; a press of the binding would open the microphone,
  /// and the words would land in a draft nobody can see.
  it('starts no take while a machine turn owns the box', () => {
    const { wiring: w, act, began } = wiring({ machine: () => true });
    const handlers = pushToTalkHandlers(w);
    press(handlers);
    expect(act, 'a press on a stood-down box acts on nothing').not.toHaveBeenCalled();
    expect(
      began,
      'and it does not move the keyboard into a box that is not drawn',
    ).not.toHaveBeenCalled();
  });

  /// **A machine turn refuses the begin, not the key.** A take can be in
  /// flight when the box stands down - the judgment turn's idle gate does not
  /// consider dictation, and a compaction starts whenever - so the press must
  /// still finish it: the box draws no row to end it from, and Escape
  /// discards the words.
  it('still ends a take in flight under a machine turn, in toggle mode', () => {
    const { wiring: w, act, began } = wiring({ machine: () => true, mode: () => 'toggle' }, true);
    const handlers = pushToTalkHandlers(w);

    press(handlers);

    expect(act, 'the press ends the take').toHaveBeenCalledWith('finish');
    expect(began, 'and it begins nothing').not.toHaveBeenCalled();
  });

  it('still ends a take in flight under a machine turn, in auto mode', () => {
    const { wiring: w, act, began } = wiring({ machine: () => true, mode: () => 'auto' }, true);
    const handlers = pushToTalkHandlers(w);

    press(handlers);
    expect(act, 'the press begins nothing').not.toHaveBeenCalled();

    release(handlers);
    expect(act, 'and the release ends the take').toHaveBeenCalledWith('finish');
    expect(began, 'nothing was begun at any point').not.toHaveBeenCalled();
  });

  /// **The key belongs to this client while the caret is in a box of its own
  /// that takes dictation.** Each of the three is a door - the bar's feedback
  /// line included, because it is the box still on screen when a takeover
  /// owns the page - and a predicate narrowed to any one of them is the
  /// decision about the page's keyboard going.
  it.each(['composer', 'dock', 'bar'])(
    'starts a take from the %s box while a browser takeover is up',
    (editor) => {
      focusBox(editor);
      const { wiring: w, act } = wiring();
      const handlers = pushToTalkHandlers(w);
      surface.phase = 'live';
      press(handlers);
      expect(act, `a press in the ${editor} box begins the take`).toHaveBeenCalledWith('begin');
    },
  );

  /// **A box that takes no dictation is not a door.** The page under the
  /// takeover keeps the keyboard for every other box the client names.
  it.each(['board', 'palette', 'nowhere'])(
    'starts no take from the %s box while a browser takeover is up',
    (editor) => {
      focusBox(editor);
      const { wiring: w, act, began } = wiring();
      const handlers = pushToTalkHandlers(w);
      surface.phase = 'live';
      press(handlers);
      expect(act, `a press in the ${editor} box begins nothing`).not.toHaveBeenCalled();
      expect(began, 'and the keyboard stays with the page').not.toHaveBeenCalled();
    },
  );

  /// **The stand-down is the takeover's, not the key's**: with no surface up
  /// the same press begins the take.
  it('starts the take again once the takeover is down', () => {
    const { wiring: w, act } = wiring();
    const handlers = pushToTalkHandlers(w);
    press(handlers);
    expect(act, 'a press with no takeover begins the take').toHaveBeenCalledWith('begin');
  });

  /// **The bar's line is a dictation box wherever the caret is.** It is the
  /// only text box above the page the takeover stands for, so a press made on
  /// the page itself - the caret on the body, where a reader driving the page
  /// leaves it - belongs to the bar's take rather than being swallowed by the
  /// takeover's keyboard stand-down.
  it('starts the bar take from a caret outside every box while a takeover is up', () => {
    const { wiring: w, act, began } = wiring({ barLine: () => true });
    const handlers = pushToTalkHandlers(w);
    surface.phase = 'live';
    press(handlers);
    expect(act, 'a press with the caret on the page begins the bar take').toHaveBeenCalledWith(
      'begin',
    );
    expect(began, 'and the keyboard is moved into the bar line').toHaveBeenCalledTimes(1);
  });

  /// **A take already in flight still ends on the key**, even under a takeover
  /// that stands for no ask: the reader cannot see the bar's row for it, and
  /// Escape would throw the words away.
  it('ends a live take under a takeover the bar does not take dictation for', () => {
    const { wiring: w, act } = wiring({ mode: () => 'toggle' }, true);
    const handlers = pushToTalkHandlers(w);
    surface.phase = 'live';
    press(handlers);
    expect(act, 'the press ends the live take').toHaveBeenCalledWith('finish');
  });

  /// **A take is in flight from its press, not from its open landing.** The
  /// browser's permission round trip is seconds long; a reader pressing again
  /// there means the stop, and a press that read only the landed take began
  /// nothing and left the open running.
  it('ends a take that is still opening', () => {
    const { wiring: w, act } = wiring({ mode: () => 'toggle' }, false, true);
    const handlers = pushToTalkHandlers(w);
    press(handlers);
    expect(act, 'the press asks for the stop while the open runs').toHaveBeenCalledWith('finish');
  });

  /// **A release still lands while a takeover is up**: a press that began
  /// before it opened has a capture waiting for this release.
  it('releases a take that a takeover opened over', () => {
    const { wiring: w, act } = wiring({ mode: () => 'hold' });
    const handlers = pushToTalkHandlers(w);
    press(handlers);
    expect(act).toHaveBeenCalledWith('begin');
    act.mockClear();
    surface.phase = 'live';
    release(handlers);
    expect(act, 'the capture begun before the takeover still ends').toHaveBeenCalledWith('finish');
  });

  /// **The overlay's route is the window's route.** AppKit hands the overlay a
  /// bare modifier as its raw keycode; the shell forwards it here, and it goes
  /// into the same handlers, so the side this client binds ends the one take.
  it("routes the overlay's modifier into the same handlers", () => {
    const { wiring: w, act } = wiring({ mode: () => 'toggle' }, true);
    const handlers = pushToTalkHandlers(w);
    surface.phase = 'live';
    routeModifier(handlers, { code: 62, down: true });
    expect(act, 'the raw right Control is this platform bound key').toHaveBeenCalledWith('finish');
  });

  /// **A forwarded chord discards the take it began.** The page never hears
  /// the other key on the overlay route; the shell reports it, and without the
  /// mark the release submits a take the person meant as a shortcut.
  it('cancels a take when a key arrives under the forwarded modifier', () => {
    const { wiring: w, act } = wiring({ mode: () => 'hold' });
    const handlers = pushToTalkHandlers(w);
    routeModifier(handlers, { code: 62, down: true });
    expect(act).toHaveBeenCalledWith('begin');
    act.mockClear();
    handlers.chord();
    routeModifier(handlers, { code: 62, down: false });
    expect(act, 'the release discards the take its press began').toHaveBeenCalledWith('cancel');
  });

  /// **A bare forwarded modifier is not a chord.** The control beside the case
  /// above: with no other key the release finishes the take as it always did.
  it('finishes a bare forwarded modifier without a chord', () => {
    const { wiring: w, act } = wiring({ mode: () => 'hold' });
    const handlers = pushToTalkHandlers(w);
    routeModifier(handlers, { code: 62, down: true });
    act.mockClear();
    routeModifier(handlers, { code: 62, down: false });
    expect(act, 'a bare modifier release finishes the take').toHaveBeenCalledWith('finish');
  });

  /// **The Mac sides are the bind's.** Every other case runs jsdom's non-mac
  /// substitution; the desktop is the platform where raw 54 is the bound
  /// right Command and 55 is the other side, which must not chord.
  it('binds the Mac sides: the right Command acts and the left does not chord', () => {
    const platform = navigator.platform;
    Object.defineProperty(navigator, 'platform', { configurable: true, value: 'MacIntel' });
    try {
      const { wiring: w, act } = wiring({ mode: () => 'hold' });
      const handlers = pushToTalkHandlers(w);
      routeModifier(handlers, { code: 54, down: true });
      expect(act, 'the right Command is the bound side').toHaveBeenCalledWith('begin');
      act.mockClear();
      routeModifier(handlers, { code: 55, down: true });
      routeModifier(handlers, { code: 54, down: false });
      expect(act, 'the left Command does not chord, so the release finishes').toHaveBeenCalledWith(
        'finish',
      );
    } finally {
      Object.defineProperty(navigator, 'platform', { configurable: true, value: platform });
    }
  });

  /// **A press whose release was lost is forgotten.** The window blur and a
  /// hidden page clear it; a press kept on record would take the next one for
  /// a repeat and ignore it.
  it('forgets a press whose release was lost', () => {
    const { wiring: w, act } = wiring();
    const handlers = pushToTalkHandlers(w);
    press(handlers);
    expect(act).toHaveBeenCalledWith('begin');
    act.mockClear();
    handlers.reset();
    press(handlers);
    expect(act, 'the next press begins again').toHaveBeenCalledWith('begin');
  });

  /// **The other side's modifier is bare, not a chord.** The window route
  /// reads the DOM `key` for that; the overlay's route has to carry it too, or
  /// brushing the other Control while the key is held would discard the take
  /// the press began.
  it("leaves the other side's modifier out of a press in flight", () => {
    const { wiring: w, act } = wiring({ mode: () => 'hold' });
    const handlers = pushToTalkHandlers(w);
    press(handlers);
    act.mockClear();
    routeModifier(handlers, { code: 59, down: true });
    release(handlers);
    expect(act, 'the release still finishes rather than abandoning the take').toHaveBeenCalledWith(
      'finish',
    );
  });
});
