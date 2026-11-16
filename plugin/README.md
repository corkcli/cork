# Cork MCP for Claude Code: multiple Claude Code agents, one repo, no conflicts

[![License: MIT](https://img.shields.io/badge/license-MIT-ffc61a.svg)](LICENSE)
[![Claude Code plugin](https://img.shields.io/badge/Claude%20Code-plugin-111.svg)](#install)
[![MCP](https://img.shields.io/badge/MCP-server-ffc61a.svg)](https://corkcli.com/docs)
[![coordinated by cork](https://corkcli.com/badge.svg)](https://corkcli.com)

**Cork is the coordination layer for multiplayer coding agents: it keeps Claude Code, Codex and Cursor agents, on one machine or across a team's machines, from overwriting each other on one codebase, and hands each agent what the others already learned.**

This is Cork's open-source Claude Code plugin: the hooks, skill and commands. Run several Claude Code agents on one repository, alone or with a team, without them tripping over each other.

- **Each agent starts in the right place.** When you type a prompt, the agent is handed the exact functions it is about: their signatures, who calls them, and what changed recently. Less searching.
- **Agents hear about each other's work.** When a teammate's agent renames a function yours uses, or starts writing the module your task needs, your agent is told on its next step.
- **Writes that would clash are stopped.** An edit against a function a teammate just changed is held until your agent has their version.
- **Pushing takes one step.** `git push` is rebased onto your teammates' new commits, tested and pushed for the agent, instead of the reject-pull-retest loop.
- **Agents can talk across teams.** In Hubs, your agents message another Cork user's agents by username (`@kendra`, or one agent with `@kendra/2`) once that person accepts. Only what agents choose to send goes there; code reports never do.
- **Notes stay with the code.** What an agent said about a change it finished is kept on the functions it touched and handed to the next agent there.

In our benchmark (12 agents at once on one repository, hard tasks), agents with Cork used 53% fewer tokens than the same agents without it. [The study](https://corkcli.com/benchmarks/study-5-twelve-agents-53-percent).

## Install

In Claude Code, send these as two separate messages:

```
/plugin marketplace add corkcli/cork
```
```
/plugin install cork@cork
```

Installed before the rename, as `collide@collide`? Run `/plugin uninstall collide@collide`, then the two lines above: the machine keeps its account and its history (its `~/.collide` folder becomes `~/.cork` on macOS and Linux, and the old path keeps working).

In Codex:

```
codex plugin marketplace add corkcli/cork
codex plugin add cork@cork
```

In Codex the plugin brings Cork's skills and its MCP server, but not these hooks (they are written for Claude Code). Codex gets Cork's hooks from the machine install, `npx corkcli`, or from the install command Cork's `setup` tool hands your agent.

That is all. The next session sets Cork up for every repository on this machine: free, no account, and nothing of yours leaves the machine. Run as many agents as you like in as many repositories as you like; agents in the same repository see each other's work and can message each other.

`/cork:status` shows what Cork did on this machine. `/cork:login` links the machine to a Cork account: on Cloud Free or the paid plan, the machine moves to Cork's servers by itself and brings its history along, so your agents on your other signed-in machines join in; on the paid plan, teammates' agents on their machines do too.

## What is in this repository

- `hooks/hooks.json`: the Claude Code hooks (session start, prompt, every read, edit and command, failed calls, stop), all run through `bin/cork`.
- `bin/cork`: the launcher. It fetches the pinned `cork` program once and runs it; it fails open. (`bin/collide` is the same launcher under its name from before the rename.)
- `skills/cork` (the `cork` skill): teaches your agent how to read what Cork tells it (briefings, Δ lines, blocked writes, landings) and to say when a teammate's work changed its plan.
- `skills/login` and `skills/status`: sign the machine in and see what Cork is doing (`/cork:login` and `/cork:status` in Claude Code).

- `.mcp.json`: Cork's hosted MCP server, `https://mcp.corkcli.com` (look up any symbol, blast radius, team memory). It asks you to sign in the first time an agent uses it; the hooks work without it.

## What it sends, and what it keeps

On the free version, nothing: everything stays in `~/.cork/local` on your machine. A signed-in machine works on Cork's servers for repositories with a hosted remote (any other repository stays on the machine), on Cloud Free and on the paid plan alike, and this is what reaches Cork:

- Your code's **structure**: function and class names, signatures, docstrings, which function calls which, and a hash per line. Parsed on your machine; your source files are not sent.
- What your agents are **doing**: which files they read and edit, the commands they run (shortened, with secrets and local paths taken out), and when.
- Agents' **closing messages** and **session titles** (your workspace can switch either off), and the messages and notes agents write.
- Your **prompts** are used once to find the code they are about, then dropped. Only a numeric summary of the prompt's meaning is kept, for two hours.

`cork audit` shows every request this machine sent, from a log that stays on it. The full list: [What Cork sends](https://corkcli.com/data).

Your sign-in credential stays in `~/.cork/credentials.json` on your machine and is never written into the repository.

## How it runs

Every hook goes through `bin/cork`, which runs Cork's native `cork` program. On first use it downloads Cork (one program: the hooks and the local server) pinned in `hooks.lock` for your platform, and refuses any file whose SHA-256 does not match; later updates must carry Cork's signature. Nothing runs through Python or Node. If a binary cannot run, the hooks stay silent and never block your agent.

The first session sets up the free version for the whole machine (`cork install --no-settings`): it places the program in `~/.cork/bin`, writes `~/.cork/machine.json` and starts the local server. It leaves Claude Code's settings alone, since this plugin brings those hooks. Where Codex, Cursor or Hermes is installed, it also adds Cork's hooks and its local MCP server to their user settings (`~/.codex/hooks.json` and `~/.codex/config.toml`, `~/.cursor/hooks.json` and `~/.cursor/mcp.json`, a plugin under `~/.hermes/plugins`), so agents there see the same work. Codex asks you to trust its hooks before they run. `cork uninstall` takes Cork's hooks back out (your local data stays), and `cork install --harness <tool>` puts one tool back.

The local server listens on 127.0.0.1 only and keeps its data in `~/.cork/local`. A repository still on Cork's retired per-repo setup (hooks committed under `.collide/`) keeps them until it is migrated (`cork migrate`, or the install `setup` hands out, run at its root); the plugin stands down there meanwhile so nothing is reported twice.

## Badge

Show that a repository's agents work together through Cork: paste this into its README.

```markdown
[![coordinated by cork](https://corkcli.com/badge.svg)](https://corkcli.com)
```

Pull requests your agents open with `gh pr create` get the same line, added once by Cork's hooks with your own `gh` (take it out and it stays out). `cork badge off` stops that on this machine (`cork badge on` brings it back), and so does `CORK_PR_BADGE=0` in the environment.

## Plans

Free is everything on one machine: unlimited agents and repositories, no account. Cloud Free, with an account, is 10 agent hours a month, and your agents on all your signed-in machines see each other. Paid agent hours, $20 a month per 100 pooled across the workspace, add teammates' machines and the dashboard, and match prompts to code by meaning ([pricing](https://corkcli.com/pricing)).

## License

This plugin (the hooks, the launcher and the commands in this repository) is MIT licensed. The `cork` program it downloads is built from the source in [corkcli/cork](https://github.com/corkcli/cork): its hooks and code parser are MIT, its local engine FSL-1.1-MIT (each version becomes MIT two years after release). Cork's cloud service is not open source; it is free to use under Cork's terms.

## Frequently asked questions

**How do I run multiple Claude Code agents on the same repository?**
Install this plugin (two messages, above) and start your agents as usual. They see each other's changes as they work, clashing writes are held, and pushes land in one step.

**Do I need the MCP server too?**
No. The hooks do the work with no extra model calls. The MCP server, which the plugin adds, gives an agent tools it can call on purpose; it asks you to sign in the first time one is used.

**Does it work with Codex and Cursor?**
Yes. The first session also sets Cork up for Codex and Cursor where they are installed (above). Codex can install this plugin too, for the skills and the MCP server. Without Claude Code, install Cork for the whole machine: `curl -fsSL https://corkcli.com/install.sh | sh` or `npx corkcli` ([corkcli/cork](https://github.com/corkcli/cork)).

---

If Cork saved your agents a merge conflict, a **star** helps other developers running parallel Claude Code agents find it. Website: [corkcli.com](https://corkcli.com) · Free version and source: [corkcli/cork](https://github.com/corkcli/cork)
