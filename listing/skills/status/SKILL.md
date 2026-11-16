---
name: status
description: Show what Cork is doing on this machine and whether this repository is connected. Use when the user asks whether Cork is on, working, connected, or what it saved.
---

# Cork status

Run this in a shell and summarize the result for the user in one or two sentences:

```sh
~/.cork/bin/cork status
```

The program is Cork's machine install, `~/.cork/bin/cork` (`cork` on the PATH in a shell opened after the install). If it is not there, Cork is not set up on this machine yet: use the cork-setup skill, which asks the user first.

