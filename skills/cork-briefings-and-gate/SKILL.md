---
name: cork-briefings-and-gate
description: How to act on what Cork's hooks put in your context while other agents and people work in the same repository - the Cork briefing, lines starting with Δ, "Cork in flight" notices, blocked or held writes, semantic lint, reserved file numbers and one-step pushes. Use whenever a "Cork briefing", a Δ line, "Cork blocked this write", "Cork held this write" or "Cork landed" appears, or when two agents' work is said to collide.
---

# Working with Cork's briefings and gate

Cork's hooks run between your steps and put what the rest of the team is doing
into your context. You do not call anything for that: read what arrives and
act on it. While the hooks run, do not call `declare_intent`,
`check_collisions`, `report_edit` or `heartbeat`; the hooks do their work.

## What arrives, and what to do

- **Cork briefing** (at session start and with each prompt): the exact
  signatures, callers, recent changes and notes for the code your task is
  about. Start from it instead of searching. Each symbol carries its line
  range: to edit, read only that range. Its Scope line says whether the
  callers listed are all of them.
- **Lines starting with Δ** in a tool result: a teammate changed something
  you are working on since your last step. Adapt to it; never undo it.
- **Cork in flight**, or "... just started": work you depend on exists but is
  not pushed yet. Build against the interface given, or keep to your own part
  until it lands. Do not write your own copy of it.
- **Cork blocked this write**: the reason names the teammate's live change, or
  a numbered file (a migration, say) whose number is taken, with the one now
  reserved for you. Adapt to it, then retry; the retry goes through once your
  write accounts for it. If you both need the same lines, message them
  (`cork message --to <them> "..."`) and agree who changes what.
- **Cork held this write**: an owner already decided this code's fate. Follow
  the decision unless the user's current request clearly overrides it; then
  tell the user so, and retry.
- **The repo's check**: every edit is reported and the repository's check
  runs after writes. Its verdict arrives in your next tool result.
- **Semantic lint**: a teammate renamed, removed or re-signatured a symbol you
  use. Apply a mechanical fix yourself. "Not on origin yet" means their change
  has not landed: keep the old name until it has.
- **Two agents' work collides** (`reconciliations_pending`): call
  `pending_reconciliations`, merge by keeping both sides' work, never by
  deleting either, and call it again with `resolve` once the tests pass.

## Decisions that should outlast the session

When the user states a standing decision ("remove X", "never Y", "only Z"),
record it with `remember(kind="decision", fact=..., anchor=...)`, anchored to
the code it covers (`dir/`, a file, or `path::symbol`). Every other agent then
meets it before editing there. For a plain fact or gotcha, use `remember`
with an anchor and no kind.

## Tools worth calling

Call a Cork MCP tool only when it adds something the briefing lacks:

- `get_symbol`: exact facts about one symbol or module.
- `blast_radius`: every dependent, before a rename, a signature change or a
  delete. Skip it when the briefing's Scope line says every dependent is
  listed.
- `claim`: a number in a sequence before you write the file (the hooks
  already claim a numbered file's number when you create it).

Each takes `repo_id`: the repository's git origin as `host/owner/name`, for
example `github.com/acme/web`.

## Committing and pushing

- With Cork, a plain `git push` rebases onto teammates' new commits, runs the
  tests and pushes in one step. Read the one-line result: "It is pushed;
  nothing else to do" means exactly that. Only a conflict or a failing test
  needs you. Do not pull by hand first.
- In a checkout other sessions also write in, `cork commit --mine -m "..."`
  commits only the lines this session wrote (`--dry-run` shows them first).

## Saying what changed your plan

When something Cork told you changes what you do, say so to the user in one
plain sentence that names whose work it was: "bob just renamed
`settled_total` to `paid_total`, so I'm updating the callers to match." Say
nothing about notices that changed nothing.

## No briefing in your context

Then the hooks are not running on this machine. Without them, call
`get_briefing` before changing code, `check_collisions` before writing and
`report_edit` after. To install the hooks, call `setup` once and run the one
command it returns in a shell (or see the `cork-setup` skill). From the next
session on, the notices above arrive by themselves.
