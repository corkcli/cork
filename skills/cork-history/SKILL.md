---
name: cork-history
description: Read what Cork recorded about a repository and its agents - what each agent session did and what it cost (recap), why code is the way it is (explain_code), the team's saved notes and decisions (recall), today's summary, this session's uncommitted lines and every request Cork sent. Use when the user asks what happened, who changed something and why, what the agents did today or what it cost, before reaching for git log.
---

# Reading what Cork recorded

Cork keeps a record of every agent's edits, with the reasons given, the notes
and decisions the team saved, and the cost of each session. Ask Cork before
`git log`: it includes work not yet committed, and why it was done.

## In the repository (Cork's MCP tools)

Each takes `repo_id`: the repository's git origin as `host/owner/name`, for
example `github.com/acme/web`. A repository with no remote is
`local/<folder>`.

- `recap(repo_id, days)`: what each agent did, session by session: edits,
  lines, tokens, model, estimated cost, rationales and top paths. `days` goes
  up to 90 and defaults to 1. Use it for "what happened" and "what did this
  cost".
- `explain_code(repo_id, path, symbol)`: why code is the way it is: the
  history of a file, or of one symbol in it, oldest first. Each edit (who,
  when, what changed), completed intents with their rationales, reverted
  attempts and anchored notes, uncommitted work included.
- `recall(repo_id, query, tags)`: the team's saved notes and decisions,
  newest first. The briefing already carries the notes anchored to the code
  you touch; use this to search beyond them, before re-deciding something the
  team may already have settled.
- `get_symbol` and `blast_radius`: what the code is now, and what depends on
  it.

All four are read-only.

## On this machine (the cork command)

- `cork today`: the user's day: what their agents did, what Cork caught, and
  what is left open (`--json` for the raw data).
- `cork status`: what Cork did on this machine, and whether it is running.
- `cork status --mine`: what this session wrote that is not committed yet.
  `cork commit --mine -m "..."` commits only those lines.
- `cork message --status`: the messages you sent, and when each was delivered
  and seen.
- `cork audit`: exactly what this machine sent to Cork, kept on the machine
  and never uploaded (`--since 1h`, `-n N`, `--full`, `--json`).

## Answering

Answer with what the record says, and name whose work it was and why they did
it. When the record has no answer (code changed before Cork was installed, for
example), say so, then fall back to `git log` and `git blame`.
