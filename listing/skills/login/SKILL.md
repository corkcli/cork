---
name: login
description: "Sign this machine in to Cork, linking it to the user's Cork account. Use when the user asks to sign in, connect or link Cork, or when a session says the repository is not connected. Optional: Cork's free version needs no account."
---

# Sign this machine in to Cork

The free version already works without an account: agents on this machine see each other's work and can message each other. Signing in links the machine to the user's Cork account, so that when their workspace is on Team the machine switches to it by itself and brings its history along.

Run this command in a shell and wait for it to finish. It opens a browser for the user to sign in and waits up to five minutes:

```sh
~/.cork/bin/cork login
```

The program is Cork's machine install, `~/.cork/bin/cork` (`cork` on the PATH in a shell opened after the install). If it is not there, Cork is not set up on this machine yet: use the cork-setup skill, which asks the user first.

When it finishes, tell the user in one or two sentences what it printed: which account and workspace the machine now works with (on any plan; Free is metered in agent hours), so their agents on every machine signed in to it see each other's work. Never print or repeat a token.
