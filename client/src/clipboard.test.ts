// @vitest-environment jsdom
/**
 * The clipboard every landing writes through: the shell's plugin where the
 * page runs inside the app, the browser's clipboard where an origin allows
 * one, and a copied selection where it does not. Silent on every path.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

const { writeText } = vi.hoisted(() => ({ writeText: vi.fn() }));
vi.mock('@tauri-apps/plugin-clipboard-manager', () => ({ writeText }));

import { copyToClipboard } from './clipboard';

/** Pretend this page runs inside the shell, so the plugin path is taken. */
function inShell(on: boolean): void {
  const held = window as unknown as Record<string, unknown>;
  if (on) held['__TAURI_INTERNALS__'] = {};
  else delete held['__TAURI_INTERNALS__'];
}

/** The clipboard jsdom does not carry, where a case wants one. */
function withClipboard(write: (text: string) => Promise<void>): ReturnType<typeof vi.fn> {
  const stub = vi.fn(write);
  Object.defineProperty(navigator, 'clipboard', {
    configurable: true,
    value: { writeText: stub },
  });
  return stub;
}

/** The selection copy jsdom does not carry either. `copied` is the box's text
 *  and the span it had selected at the moment the copy ran: `execCommand`
 *  copies the selection, so an unselected box copies nothing however right
 *  its text is. */
function withExecCommand(): {
  exec: ReturnType<typeof vi.fn>;
  copied: { text: string; from: number | null; to: number | null }[];
} {
  const copied: { text: string; from: number | null; to: number | null }[] = [];
  const exec = vi.fn(() => {
    const box = document.querySelector('textarea');
    copied.push({
      text: box?.value ?? '',
      from: box === null ? null : box.selectionStart,
      to: box === null ? null : box.selectionEnd,
    });
    return true;
  });
  Object.defineProperty(document, 'execCommand', { configurable: true, value: exec });
  return { exec, copied };
}

beforeEach(() => {
  writeText.mockReset();
  writeText.mockResolvedValue(undefined);
  inShell(false);
  delete (navigator as unknown as { clipboard?: unknown }).clipboard;
  Reflect.deleteProperty(document, 'execCommand');
  document.body.innerHTML = '';
});

afterEach(() => {
  inShell(false);
  delete (navigator as unknown as { clipboard?: unknown }).clipboard;
  Reflect.deleteProperty(document, 'execCommand');
  document.body.innerHTML = '';
});

describe('the landing clipboard', () => {
  it("writes through the shell's plugin where the page runs inside the app", async () => {
    inShell(true);
    const clipboardWrite = withClipboard(() => Promise.resolve());
    const { exec } = withExecCommand();

    await copyToClipboard('say staging, not prod');

    expect(writeText, 'the plugin is the shell path').toHaveBeenCalledTimes(1);
    expect(writeText).toHaveBeenCalledWith('say staging, not prod');
    expect(
      clipboardWrite,
      'the plugin path never touches the browser clipboard',
    ).not.toHaveBeenCalled();
    expect(exec, 'nothing falls through to a selection copy').not.toHaveBeenCalled();
  });

  it('writes through navigator.clipboard in a browser where the origin allows one', async () => {
    const clipboardWrite = withClipboard(() => Promise.resolve());
    const { exec } = withExecCommand();

    await copyToClipboard('say staging, not prod');

    expect(clipboardWrite, 'the browser clipboard is the web path').toHaveBeenCalledTimes(1);
    expect(clipboardWrite).toHaveBeenCalledWith('say staging, not prod');
    expect(writeText, 'a browser page has no shell plugin').not.toHaveBeenCalled();
    expect(exec, 'the fallback is only for a page with no clipboard').not.toHaveBeenCalled();
  });

  it('copies a hidden selection where the origin offers no clipboard', async () => {
    const { exec, copied } = withExecCommand();

    await copyToClipboard('say staging, not prod');

    expect(writeText, 'a browser page has no shell plugin').not.toHaveBeenCalled();
    expect(exec, 'the last best-effort path is a selection copy').toHaveBeenCalledWith('copy');
    expect(copied, 'the box held the words, all of them selected').toEqual([
      { text: 'say staging, not prod', from: 0, to: 'say staging, not prod'.length },
    ]);
    expect(document.querySelector('textarea'), 'the box does not outlive the copy').toBeNull();
  });

  it("resolves when the shell's plugin refuses the write", async () => {
    inShell(true);
    writeText.mockRejectedValue(new Error('refused'));
    const clipboardWrite = withClipboard(() => Promise.resolve());

    await expect(copyToClipboard('say staging, not prod')).resolves.toBeUndefined();

    expect(
      clipboardWrite,
      'a refused plugin is not retried through the browser',
    ).not.toHaveBeenCalled();
    expect(document.body.innerHTML, 'a refusal draws nothing').toBe('');
  });

  it("resolves when the browser's clipboard refuses the write", async () => {
    withClipboard(() => Promise.reject(new Error('refused')));

    await expect(copyToClipboard('say staging, not prod')).resolves.toBeUndefined();

    expect(document.body.innerHTML, 'a refusal draws nothing').toBe('');
  });

  it('resolves when the selection copy throws', async () => {
    Object.defineProperty(document, 'execCommand', {
      configurable: true,
      value: () => {
        throw new Error('refused');
      },
    });

    await expect(copyToClipboard('say staging, not prod')).resolves.toBeUndefined();

    expect(
      document.querySelector('textarea'),
      'the box is removed on the throwing path',
    ).toBeNull();
    expect(document.body.innerHTML, 'a refusal draws nothing').toBe('');
  });
});
