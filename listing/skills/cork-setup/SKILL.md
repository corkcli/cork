---
name: cork-setup
description: Set Cork up so several coding agents (Claude Code, Codex, Cursor and others) can work on one repository without overwriting each other. Use when the user asks to install, set up, connect, update or remove Cork, when a repository that should be covered has no "Cork briefing" in the context, or when a Cork tool says the hooks are not installed.
---

# Set Cork up

Cork coordinates the coding agents that work on one repository at the same
time: each is briefed on the code it is about to touch, told when another
agent changes what it depends on, and held before a write that would clash.
One install covers every repository on the machine and writes nothing into
them. The free version needs no account.

## Install

Ask the user before installing anything. Then run this in a shell (any
terminal, including Windows PowerShell) and wait for it to finish:

```sh
npx corkcli
```

It comes from npm, and it checks the `cork` build it downloads against its
published SHA-256 and Cork's Ed25519 release signature before running it. On a machine without Node,
send the user to https://corkcli.com/install for the other ways to install;
don't pipe a downloaded script into a shell on their behalf.

It puts the `cork` program in `~/.cork/bin/cork`, adds that folder to the
PATH in the shell's startup files, and installs Cork's hooks for Claude Code,
Codex, Cursor and Hermes wherever they are installed. Agents already open pick
it up at their next prompt. In a shell opened before the install, run
`~/.cork/bin/cork` by its full path.

From inside Claude Code, the plugin does the same:

```
/plugin marketplace add corkcli/cork
/plugin install cork@cork
```

For Codex, `codex plugin marketplace add corkcli/cork` and then
`codex plugin add cork@cork` bring Cork's skills and MCP server. Codex's hooks
come from the machine install above.

## Check that it works

Run `cork status` and tell the user what it says in a sentence or two: what
Cork did on this machine, and whether it is running. From the next prompt on,
each agent session gets a **Cork briefing**.

## In the repository

- `cork agents-md` adds Cork's section to the repository's AGENTS.md, after
  everything already there, so agents in tools without hooks follow the same
  rules. Run it only when the user wants that, and commit the change like any
  other.
- `cork migrate` retires a repository's old per-repo setup (`.collide/` hook
  files).

## An account and a team (optional)

- `cork login` links this machine to a Cork account. It opens a browser and
  waits up to five minutes for the user to sign in.
- `cork upgrade` adds units for the workspace (100 agent hours a month each,
  pooled).
- `cork add` shares this repository with the workspace. The repository needs
  a remote on GitHub or another git host.

## Tools without hooks

Windsurf, Cline and GitHub Copilot use Cork through its MCP server,
`https://mcp.corkcli.com` (OAuth sign-in, no key). Its `connect_agents` tool,
with `mode` set to `"files"`, returns the rules files and MCP configs to
commit for them.

## Keep it current, or take it out

- `cork update` gets the newest version now; `cork rollback` goes back to the
  one before the last update.
- `cork uninstall` takes Cork's hooks out. The local data stays.
