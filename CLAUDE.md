# CLAUDE.md

**Read [`AGENTS.md`](AGENTS.md) before you touch anything in `src/`.** It is the canonical
instructions for this repository — how to build and test, what the gate is, the safety invariants
that are not negotiable, and the traps that have already cost someone a day.

This file exists only to point at it. Sheep is deliberately cross-harness — it records `claude`,
`codex`, `opencode` and everything else herdr attributes an agent to, in one format — so its
working instructions live in the file every tool reads, not in a vendor-named one. Keeping the
content here as well would mean two files to update and one of them silently wrong; a pointer
cannot drift.

The one thing worth repeating here, because it is what the rest is for: **Sheep overwrites files on
other people's machines.** A change that can write to a user's files needs a test that fails
without it. [`CONTRIBUTING.md`](CONTRIBUTING.md) is the bar in full.
