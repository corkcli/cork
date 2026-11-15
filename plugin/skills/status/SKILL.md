---
name: status
description: Show what Cork is doing on this machine and whether this repository is connected. Use when the user asks whether Cork is on, working, connected, or what it saved.
---

# Cork status

Run this in a shell and summarize the result for the user in one or two sentences:

```sh
"${CLAUDE_PLUGIN_ROOT}/bin/cork" status
```

The program is `bin/cork` in this plugin's folder, two levels above this skill's folder; if the path above comes out empty, run it from there.

Installed as a skill without the plugin, there is no plugin folder: run `cork status` instead (Cork's machine install, `npx corkcli`, puts `cork` in `~/.cork/bin`).
