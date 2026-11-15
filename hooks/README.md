# cork-hooks

Cork's agent-side components as one native program. It is installed once
per machine as `~/.cork/bin/cork` (by `corkcli.com/install.sh`, the
command Cork's `setup` tool hands out), registered at the user level of
each agent tool (`~/.claude/settings.json`, `~/.codex/hooks.json`,
`~/.cursor/hooks.json`, a Hermes plugin), and it updates itself. Nothing is
committed into a repo for it, and no Python interpreter is involved.

## Installs from before the rename

Cork was called Collide. Everything a machine installed then keeps working,
and nothing is moved:

- **Data folder**: `~/.cork`, except that a machine with `~/.collide` and no
  `~/.cork` keeps `~/.collide` (`config::data_dir`; the local server picks
  the same). `cork install` on such a machine also settles a stray `~/.cork`
  that only an installer's download created (`machine::settle_layout`).
- **Program**: an old install keeps `~/.collide/bin/collide`, which its hook
  entries run; the install, the first session of a newer program and every
  self-update place a `cork` beside it (`machine::place_cork_beside`,
  `selfupdate::bring_twin_along`), so `cork ...` works everywhere. Releases
  keep their names and signatures (`collide-<target>`, "collide-hook <target>
  v<N> sha256:..."), whatever the program is called on the machine.
- **Settings**: `CORK_X` is read first, then `COLLIDE_X` (`config::get`).
- **Hook entries**: new ones carry `# cork-machine` and run `~/.cork/bin/cork`;
  an old install's entries stay byte for byte (`# collide-machine`, the old
  Codex labels), because Codex trusts each hook by its hash. Either mark is
  recognised, replaced and removed.
- **MCP**: registered as `cork`; a `collide` entry (ours or added by hand) is
  replaced, never kept beside it.
- **AGENTS.md**: the section is written between `<!-- cork:start -->` and
  `<!-- cork:end -->`; one under the old `collide` markers is replaced in
  place, never doubled.
- **Repo config and credentials**: `.cork/config.json`, else
  `.collide/config.json`; the credential in either data folder.
- **Background services**: `com.corkcli.wake` (launchd), `cork-wake.service`,
  `CorkWake`, and the Hermes plugin `cork`; the old ones are taken out first.
- **User-Agent**: `cork-report-hook/<version>`.

| Subcommand | Fires |
|---|---|
| `gate` | PreToolUse, before every Edit/Write/shell write |
| `report` | PostToolUse on every tool call, Stop, SessionStart, UserPromptSubmit, SessionEnd |
| `pre-commit` | the git pre-commit hook |
| `watch` | the disk watcher, for humans editing without an agent |
| `install` / `uninstall` / `login` / `status` | the machine install (machine.rs) |
| `migrate` | retire a repo's old per-repo setup (see below) |

## Why this is where Rust pays

These are not hot loops, they are hot *processes*. The gate runs before every
write with a hard 200 ms fail-open budget, and a Python interpreter spent
about 33 ms of that budget merely booting. The reporter fires on every single
tool call an agent makes.

Measured on this machine, same input, same work:

| | Startup to decision |
|---|---|
| `python gate_hook.py` (retired) | 34.4 ms |
| `collide-hook gate` | 2.0 ms |

## Tested end to end

`server/tests/test_rust_hooks.py` and `test_hook_responses.py` drive this
binary against a fake server and assert the exit code, stdout, stderr and the
exact request body it sent: allow and block, fail-open when the server is
down, repo-root discovery from a subdirectory, shell-write detection,
`apply_patch` across several files, out-of-repo files, usage-limit reporting,
turn cost from the transcript, and the credential resolution order.
`server/scripts/hook_smoke.py` plays one session through the exact machine
hook commands on every platform, and `free_smoke.py` the whole free version.

A hook decides whether a write is blocked and what teammates see, so a
behavioural difference here is a correctness bug, not a performance footnote.

## The retired per-repo setup

Cork used to be set up per repo: `setup` committed hook scripts, a pinned
binary and a config under `.collide/`, and wired them from the repo's own
`.claude/settings.json` (and `.codex/hooks.json`, `.cursor/hooks.json`). That
mode is gone. For repos that still carry it:

- while the repo's own hooks are wired, runnable and configured, the
  machine's hooks stand down there (`machine::repo_runs_its_own`), so no
  event is handled twice;
- the hook fingerprints Cork commands in the repo's own settings
  (`settings_digest`), and the server answers a non-empty one with
  `repo_hooks_retired`, telling the agent to migrate;
- `cork install` run at the repo's root (what `setup` hands out) and
  `cork migrate` remove the setup (`machine::retire_repo_setup`): the
  `.collide/` hook files, Cork's entries in each settings file, the
  pre-commit wiring and the `.gitignore` line. The repo's own choices in
  `.collide/config.json` (`verify`, `auto_pull`, `land`, `protocol`) stay. The
  change is staged for the person to commit.

`.cork/config.json` (or `.collide/config.json`) is still read when present, so a repo mid-migration
keeps reporting where it did.

## Fail open, in Rust

A `catch_unwind` around the whole dispatch exits 0, plus `Result` at every
I/O edge. The one deliberate non-zero exit is the gate returning 2 for a real
block, and the pre-commit gate returning 1.

## Watching

```sh
cork watch --once                 # one pass, for CI or cron
cork watch --interval 5           # poll forever
```

Server, token and repo id come from the machine's install (or a repo's
`.cork/config.json`) and `~/.cork/credentials.json` when not passed as
flags, exactly as the hooks resolve them. Reports carry the `collide-watch`
identity, which is what marks an edit disk-confirmed rather than
agent-claimed.
