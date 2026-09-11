# Slack connector

The `[[slack]]` connector lets a session act as the user on Slack: it watches the conversations a subscription names, delivers what matches into the owning session, and holds every outbound action for the user's decision. This page covers the connector's agent-facing surfaces. The SLACK section of the [Inspector](./inspector.md) shows the per-workspace pump liveness, and the unified prompt in [Input](./input.md) shows the approval the outbound tools share.

## Tools

The in-process MCP server exposes the `mcp__forge__slack__*` tools to every session, whichever workspace configuration exists; a call naming no configured workspace fails with the labels that are configured.

| Tool | What it does |
|---|---|
| `slack__list` | Every conversation the token's user is in, each row marked with whether YOU subscribe to it. An optional `name` substring filters over name, purpose and topic, and an optional `kind` filter keeps one conversation type (`public`, `private`, `im`, `mpim`). |
| `slack__subscribe` | Watch the whole DM class, named conversations each with a mode (`all` or `mentions`), or the workspace-wide mention target. |
| `slack__unsubscribe` | Drop one of the caller's own subscriptions by id. |
| `slack__post` | Post a message as the user, as a root message or into a thread. Text past 4000 characters is split into numbered parts. Held for approval. |
| `slack__edit` | Replace or delete one of the user's own messages. Held for approval. |
| `slack__react` | Add or remove a reaction. Held for approval. |
| `slack__attachment` | Fetch a Slack file to a local directory, or upload a local file into a conversation. Fetching is not held; an upload is. |
| `slack__search` | Workspace-wide message search, newest first. A text lookup, not a mention detector. |
| `slack__user` | One user's handle, real name and timezone. |
| `slack__pins` | A conversation's pinned messages. |
| `slack__bookmarks` | A conversation's bookmarks. |

Reads are plain calls. `slack__post`, `slack__edit`, `slack__react` and an upload are held: the tool call does not return until the user approves or rejects in the dock prompt, and a rejected or unanswered draft sends nothing.

## What a mention subscription reaches

A `mentions` subscription is swept by one workspace-wide search per tick rather than by polling every channel, which is what makes it affordable. What it reaches was measured live against a real workspace:

- A mention in a **public** channel the user has not joined is reachable.
- A **private** channel the user is not in is not reachable, matching the user's own view of the workspace.
- Search indexing lags roughly 16 seconds, so a mention is not instantaneous: the sweep finds it on a later tick.
- Results are explicitly timestamp-ordered. The API's default sort is by relevance, which surfaces stale hits first.

A matched mention starts the conversation it came from being watched by the mention subscription's owner, so the agent can reply back and forth without anyone subscribing to the channel by hand. A message the user authored is never delivered, or an agent answering in Slack would answer itself.
