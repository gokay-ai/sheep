## What this changes, and why

<!-- The reasoning, not the diff. If the change is not obvious, say what the wrong version
     would have done. -->

## Does it write to a user's files?

<!-- Any code path that removes or overwrites a path in the working tree, or that touches one
     of the eight safety invariants in AGENTS.md. If yes: which test fails without this change,
     and did you watch it fail? Assert the property, not a count. -->

- [ ] No — nothing on the write path changed.
- [ ] Yes, and the test that goes red when this change is reverted is: `…`

## Checks

- [ ] `cargo test`
- [ ] `cargo clippy --all-targets -- -D warnings`
- [ ] `cargo fmt --check`
- [ ] `shellcheck herdr-plugin/install.sh herdr-plugin/scripts/*.sh` — if the plugin half changed
- [ ] Ran on Linux / macOS (say which; CI covers both)
