# Cork: run multiple AI coding agents on one repo without conflicts

[![License: MIT](https://img.shields.io/badge/hooks-MIT-34d399.svg)](#what-is-in-this-repository)
[![npm](https://img.shields.io/npm/v/corkcli.svg?label=npx%20corkcli)](https://www.npmjs.com/package/corkcli)
[![MCP](https://img.shields.io/badge/MCP-server-34d399.svg)](https://corkcli.com/docs)
[![Works with](https://img.shields.io/badge/works%20with-Claude%20Code%20%C2%B7%20Codex%20%C2%B7%20Cursor-111.svg)](https://corkcli.com)

**Cork is the coordination layer for multiplayer coding agents: it keeps Claude Code, Codex and Cursor agents, on one machine or across a team's machines, from overwriting each other on one codebase, and hands each agent what the others already learned.**

This repository is **Cork**: run as many coding agents as you like on one machine, in as many repositories as you like, without them tripping over each other. Free, no account, and nothing of yours leaves your machine.

```sh
curl -fsSL https://corkcli.com/install.sh | sh
```

or, with Node (any terminal, including Windows PowerShell):

```sh
npx corkcli
```

One install covers every repository on the machine. Agents already open pick it up at their next prompt.

Or as a plugin. In Claude Code, send these as two separate messages:

```
/plugin marketplace add corkcli/cork
```
```
/plugin install cork@cork
```

In Codex: `codex plugin marketplace add corkcli/cork`, then `codex plugin add cork@cork`. What the plugin brings and does: [plugin/README.md](plugin/README.md).

## Why

Run two or three Claude Code agents (or Codex, or Cursor) on the same codebase and they start colliding: one renames a function another is still calling, two write the same module, a push fails and the agent burns turns pulling and re-testing. Each agent also re-reads the same files the others just read. Separate git worktrees keep their files apart, but the work still meets at merge time.

Cork sits under the agents you already run. It does not run agents or host machines. Its hooks tell each agent what the others are changing while they work, hold a write that would clash, and hand each prompt the exact code it is about, so agents search less.

In Cork's benchmark (12 Claude Code agents at once on one repository, hard tasks), agents with Cork used **53% fewer tokens** than the same agents without it. [The study](https://corkcli.com/benchmarks/study-5-twelve-agents-53-percent) · [all benchmarks](https://corkcli.com/benchmarks)

## What your agents get

- **A briefing before they start.** The functions a prompt is about, with their signatures, who calls them and what changed recently, handed to the agent before it searches.
- **Each other's work as it happens.** When another agent renames a function yours uses, or writes the module your task needs, your agent hears about it on its next step.
- **Writes that would clash are held.** An edit against code another agent just changed waits until your agent has their version.
- **Lint from teammates' changes.** A rename or a new signature becomes a rule your agents are checked against.
- **One-step pushes.** `git push` brings in the other agents' commits, runs your tests on the result and pushes, instead of the reject, pull, retry loop.
- **Messages between agents.** `cork message "..."` reaches the other agents in the repository, or one of them (`--to`), or whoever is on a piece of code (`--about path.py::symbol`).
- **Notes that stay with the code.** When an agent finishes a change, what it said about the change is kept on the functions it touched, and the next agent there is handed it.
- **MCP tools.** `get_symbol`, `blast_radius`, `recap`, `remember` and more, from the same code graph the hooks keep.
- **What it saved.** Once a day your agent's terminal shows the tokens Cork saved on this machine.

## How it compares

| | Cork | Git worktrees | Agent orchestrators and cloud agents |
|---|---|---|---|
| Where agents run | Where they already run | Where they already run | On their servers or VMs |
| Agents see each other's changes while working | Yes | No, only at merge | Varies |
| Holds a write that would clash | Yes | No | No |
| Briefs each agent on the code its prompt is about | Yes | No | Varies |
| Works with Claude Code, Codex and Cursor together | Yes | Yes | Varies |

They combine: agents in separate worktrees still benefit from Cork telling them what the others are changing. Detailed comparisons: [corkcli.com/blog/compare](https://corkcli.com/blog/compare).

## Commands

| Command | What it does |
|---|---|
| `cork status` | What Cork did on this machine, and where its data is |
| `cork message "..."` | Tell the other agents in this repository something |
| `cork agents-md` | Add Cork's section to this repository's AGENTS.md, after what is already there |
| `cork login` | Link this machine to a Cork account |
| `cork upgrade` | Units for your workspace: 100 agent hours a month each, pooled |
| `cork uninstall` | Take Cork's hooks out (your local data stays) |

## Where your data is

Everything stays in `~/.cork/local` (`~/.collide/local` on a machine set up before the rename) on your machine: a small server on `127.0.0.1` that only this machine can reach. Cork makes no calls out, except to check for a signed update of itself.

## Other machines, and a team

Free and local, Cork connects the agents on one machine. `cork login` signs the machine in to Cloud Free (10 agent hours a month): it moves to Cork's servers with nothing else to download, brings its history along, and your agents on your other signed-in machines see its work. [Cork's paid plan](https://corkcli.com/pricing) ($20 a month per 100 agent hours, pooled across the workspace) adds your teammates: their agents see your agents' work, and prompts are matched to code by meaning, not only by the names they use. `cork upgrade` opens the billing page for units.

## Frequently asked questions

**How do I run multiple Claude Code agents on the same repo without conflicts?**
Install Cork (one command above), then start your agents as usual. Each agent hears what the others are changing, a write against code another agent is changing is held until it has their version, and `git push` rebases, tests and pushes in one step. Guide: [run multiple Claude Code agents on one repo](https://corkcli.com/blog).

**Does it work with Codex and Cursor?**
Yes. The install registers Cork's hooks for Claude Code, Codex and Cursor, and its MCP server for any MCP client.

**Is Cork an MCP server?**
It is both hooks and an MCP server. The hooks do the work with no extra model calls; the MCP server adds tools an agent can call on purpose. The hosted server is `https://mcp.corkcli.com`.

**Does my code leave my machine?**
Not when it runs free and local. Signed in, on Cloud Free or the paid plan, your source files still don't: the hooks parse each file on your machine and send its structure (names, signatures, docstrings, who calls what, a hash per line). `cork audit` shows every request, and [What Cork sends](https://corkcli.com/data) lists them all.

**Is it really free?**
Yes: free with no account and no time limit, for one machine.

## Build from source

```sh
cd free && cargo build --release
./target/release/cork install
```

`free/` builds the one program, `cork`: the hooks your agents run (`hooks/`, with the parser in `core/`) and the local server (`cork local`). [AGENTS.md](AGENTS.md) is the guide for working on this code, for people and agents alike; Claude Code, Codex, Cursor and Windsurf each find their own config for it here.

## Skills for agents

Cork's official agent skills are in [`skills/`](skills): setting Cork up, working with its briefings and write gate, handing work off and messaging other agents, and reading what Cork recorded. Install them for any agent that reads skills:

```sh
npx skills add corkcli/cork
```

## Documentation

- Setup by tool: [Claude Code](https://corkcli.com/docs/claude-code) · [Codex](https://corkcli.com/docs/codex) · [Cursor](https://corkcli.com/docs/cursor) · [Gemini CLI](https://corkcli.com/docs/gemini-cli)
- Reference: [Cork CLI (the `cork` command)](https://corkcli.com/docs/cli) · [Cork MCP server and tools](https://corkcli.com/docs/mcp-tools) · [Cork API docs (REST v1)](https://corkcli.com/docs/api) · [all docs](https://corkcli.com/docs)
- Try it with no account: [the sandbox](https://corkcli.com/docs/sandbox) · Cork's docs over MCP, no sign-in: `https://corkcli.com/docs/mcp`
- Guides: [Do you need git worktrees for AI coding agents?](https://corkcli.com/guides/do-you-need-git-worktrees-for-ai-coding-agents) · [Cleaning up the worktrees agents leave behind](https://corkcli.com/guides/clean-up-git-worktrees-ai-coding-agents)
- [Changelog](https://corkcli.com/changelog) · [Benchmarks](https://corkcli.com/benchmarks) · For agents: [llms.txt](https://corkcli.com/llms.txt), [agents.md](https://corkcli.com/agents.md)

## What is in this repository

| Folder | What it is | License |
|---|---|---|
| `hooks/` | What runs in your agent sessions: briefings, the write gate, landing, messages, the MCP bridge, install | MIT |
| `core/` | The code parser and graph the hooks and the engine share | MIT |
| `free/` | The engine: the local server that keeps the map, the briefings, lint, landing and memory | [FSL-1.1-MIT](free/LICENSE.md) |
| `protocol/` | The real-time protocol the hooks speak: spec, schema, engine, domains, a reference server and a TypeScript client | MIT |
| `plugin/` | The Claude Code and Codex plugin | MIT |
| `skills/` | Cork's official agent skills | MIT |

The engine's license lets you use, change and share it for anything except offering a competing product or service; each version becomes MIT two years after its release.

---

If Cork saved your agents a merge conflict, a **star** helps other developers running parallel agents find it. Questions and ideas: [open an issue](https://github.com/corkcli/cork/issues). Website: [corkcli.com](https://corkcli.com) · Claude Code plugin: [corkcli/cork](https://github.com/corkcli/cork/tree/main/plugin)
