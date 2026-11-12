/**
 * The share sheet's wire to the shell: the files a share handed this app,
 * and the upload that puts them in a seat's inbox.
 *
 * **The shell owns the bytes.** On the phone the shell holds the socket, so
 * the page names the seat and the cached paths and the shell reads each file
 * in 64 KiB frames, reporting progress on a channel of its own. Nothing here
 * touches a file.
 */

import { canHost } from '../browser/host';
import type { SessionSlot } from '../wire/types';

/** The most one share may carry, enforced by the shell and the server too. */
export const MAX_SHARE_BYTES = 200 * 1024 * 1024;

/** One file the share handed the app: a cache path and what it holds. */
export interface SharedFile {
  path: string;
  name: string;
  size: number;
  mime: string | null;
}

/**
 * What the shell's share intake answers: the files, why there are none, or
 * that the share carried no file at all.
 *
 * **`empty` is not an error**: a SEND with no stream is a share of nothing
 * and draws the sheet's neutral line, while `error` is a share the intake
 * could not read and draws its reason.
 */
export interface Shared {
  files: SharedFile[];
  error: string | null;
  empty: boolean;
}

/**
 * One upload's progress.
 *
 * `begin` comes first: the upload's id, which a retry needs to resume it,
 * whether the server still held the previous attempt, and the offsets it
 * already has. `progress` follows once per frame with the bytes of one file
 * that have crossed.
 */
export type ShareProgress =
  | {
      kind: 'begin';
      upload_id: number;
      attempted: boolean;
      resumed: boolean;
      received: number[];
    }
  | { kind: 'progress'; file: number; sent: number; total: number };

/** What one upload answers: the absolute paths the server wrote. */
export interface Uploaded {
  paths: string[];
}

/** The upload, as the sheet calls it; a test hands its own. */
export type UploadFn = (
  seat: SessionSlot,
  files: SharedFile[],
  uploadId: number | null,
  onProgress: (progress: ShareProgress) => void,
) => Promise<Uploaded>;

/**
 * Upload one share through the shell.
 *
 * `uploadId` is the previous attempt's, when there was one: the shell asks
 * the server whether it still holds that upload and continues from its
 * offsets, or begins a new one and says which it did on the channel.
 */
export const uploadShare: UploadFn = async (seat, files, uploadId, onProgress) => {
  if (!canHost()) {
    throw new Error('sharing a file needs the app');
  }
  const { Channel, invoke } = await import('@tauri-apps/api/core');
  const channel = new Channel<ShareProgress>();
  channel.onmessage = onProgress;
  return invoke<Uploaded>('client_share_upload', { seat, files, uploadId, channel });
};

/**
 * The prompt a share sends: the person's words, a blank line, then one
 * `file:` line per uploaded path.
 *
 * **The paths are the message.** The agent reads them with its own tools, so
 * the prompt carries no attachment and no protocol change; empty words leave
 * the file lines alone rather than opening with a blank line.
 */
export function composePrompt(text: string, paths: string[]): string {
  const lines = paths.map((path) => `file: ${path}`);
  const trimmed = text.trim();
  return trimmed === '' ? lines.join('\n') : [trimmed, '', ...lines].join('\n');
}

/** A byte count as the sheet draws it, in the units the limit is stated in. */
export function sizeLabel(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}
