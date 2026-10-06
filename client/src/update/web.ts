/**
 * The release the hub image published, read from the manifest its puller
 * writes beside the app (`latest.json`, same-origin, so no CORS and no
 * second source of truth). A page nothing serves a manifest to - the dev
 * server, a bare browser tab - gets no answer, which is also the check that
 * lets the page draw the line only where it means something.
 */
export async function latestPublished(): Promise<string | null> {
  try {
    // Rooted, not relative: a session deep link would otherwise look for the
    // manifest under the session's own path, where the app's fallback
    // answers 200 with the page - a silent null rather than an answer.
    const response = await fetch('/latest.json');
    if (!response.ok) return null;
    const manifest = (await response.json()) as { version?: unknown };
    return typeof manifest.version === 'string' ? manifest.version : null;
  } catch {
    return null;
  }
}
