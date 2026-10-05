/**
 * Where a dispatch's own call sits: the index of the turn that carries it.
 *
 * The conversation is virtualised, so the row a dispatch drew may not be in
 * the DOM at all - the column has to scroll to the turn first. The walk is the
 * whole of what the column needs to know before it can: a turn not found is a
 * dispatch in history the page does not hold yet.
 */
export function turnOfDispatch(
  turns: readonly { messages: readonly unknown[] }[],
  dispatchId: string,
): number | null {
  for (const [at, turn] of turns.entries()) {
    for (const message of turn.messages) {
      const frame = message as { message?: { content?: unknown } };
      const content = frame.message?.content;
      if (!Array.isArray(content)) continue;
      for (const block of content) {
        const held = block as { type?: unknown; id?: unknown };
        if (held.type === 'tool_use' && held.id === dispatchId) return at;
      }
    }
  }
  return null;
}
