---
trigger: always_on
description: How to build, test and change Cork's open-source hooks, parser, protocol, engine and plugin
---

# Cork's open-source code

AGENTS.md at the root is the full guide: layout, licenses, build and test.
The rules that matter most:

- The cloud is not in this tree. Each cloud module the engine calls has a
  stand-in under `free/src/free/`. Never make the free build depend on the
  `cloud` feature; it is empty here.
- A test, or the part of one, that needs the cloud goes behind
  `#[cfg(feature = "cloud")]`; the rest of it keeps running in the free build.
- Hooks fail open: if the engine is down or slow, the agent's tool call
  still goes ahead.
- The hooks send a file's structure, never its text.
- CI runs `cd hooks && cargo test --release` and
  `cd free && cargo build --release && cargo test --release`.
- A new file takes the license of its folder: MIT, except `free/`
  (FSL-1.1-MIT).
- Match the code around you: its naming, idioms and comment density.
