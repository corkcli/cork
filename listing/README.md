# Cork for the Claude plugin directory

The Cork plugin as the Claude plugin directory lists it: Cork's skills and its
remote MCP server (`https://mcp.corkcli.com`). It runs nothing on your
computer by itself.

Cork keeps several coding agents (Claude Code, Codex, Cursor and others), on
one machine or across a team, from overwriting each other on one codebase,
and hands each agent what the others already learned.

- **cork-setup** installs Cork's hooks for the whole machine with
  `npx corkcli`, after asking you. That is what briefs each agent, tells it
  when another agent changes code it depends on, and holds a clashing write.
- **cork** teaches your agent to read what Cork tells it.
- **login** and **status** sign the machine in and say what Cork is doing.
- The **MCP server** looks up any symbol, its blast radius and the team's
  notes; it asks you to sign in the first time an agent uses it.

The full plugin, which sets the hooks up by itself, is in `plugin/`:
`/plugin marketplace add corkcli/cork`, then `/plugin install cork@cork`.

Website: https://corkcli.com · Docs: https://corkcli.com/docs/claude-code ·
Privacy: https://corkcli.com/privacy
