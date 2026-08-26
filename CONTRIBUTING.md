# Contributing to Sheep

Sheep restores files. The cost of being wrong is somebody's uncommitted work, so this document is
mostly about one thing: the bar for a change that can write to a user's worktree, and how to clear
it.

Everything else is short.

## A working checkout

```bash
git clone https://github.com/gokay-ai/sheep
cd sheep
cargo build
```

Rust 1.85 or newer and `git` on `PATH` is the whole list. There is no build script, no C
dependency and nothing to vendor — deliberately, because the binary has to cross-compile to five
targets without a toolchain on the user's machine.

## Running the suite

```bash
cargo test                                          # the gate
cargo clippy --all-targets -- -D warnings
cargo fmt --check
shellcheck herdr-plugin/install.sh herdr-plugin/scripts/*.sh
```

[`.github/workflows/ci.yml`](.github/workflows/ci.yml) has two jobs, and it is worth knowing which
is which. `fmt`, `clippy` and `test` run on **Linux and macOS** both. `shellcheck` and the two
plugin scripts are a single **Linux-only** job:

```bash
bash herdr-plugin/scripts/test-install.sh    # the installer and the release matrix agree on every asset name
bash herdr-plugin/scripts/test-watchd.sh     # exactly one recorder survives eight concurrent starts
```

So nothing gates `herdr-plugin/scripts/*.sh` on macOS. If you change them, run them on a Mac
yourself — `ps` and `pgrep` are exactly where BSD and GNU differ, and the recorder daemon is built
out of both.

`test-watchd.sh` builds its own plugin root and stand-in recorder and matches only on that path,
so a real `sheep watch` on your machine is neither counted nor killed by it.

Most of the suite needs nothing but `git`. The recorder and the interface are tested against a
scripted herdr and `ratatui`'s `TestBackend`, so no pseudo-terminal and no herdr session is
involved anywhere in CI.

## Trying your change by hand

**Always redirect the state directory.** Without it you are writing turns into the timelines on
your own machine:

```bash
export SHEEP_STATE_DIR=$(mktemp -d)
cd /some/scratch/repo
cargo run --manifest-path /path/to/sheep/Cargo.toml -- doctor
```

Inside herdr, a linked development checkout needs no install step: `herdr plugin link` does not run
build steps, and [`herdr-plugin/scripts/common.sh`](herdr-plugin/scripts/common.sh) falls back to
`../target/release/sheep` and then `../target/debug/sheep`, so a `cargo build --release` in the
repository root is enough for the panes and actions to find your binary.

`bash herdr-plugin/install.sh --dry-run` prints the platform, target and asset URL it would use and
touches nothing.

## The bar for anything that writes to a user's files

[`AGENTS.md`](AGENTS.md) lists eight safety invariants. They are the product. Each one has a test
written from the attacker's side — mostly in [`tests/adversarial.rs`](tests/adversarial.rs), whose
cases are the shapes a real repository actually takes: linked worktrees, unresolved conflicts,
mid-rebase, gitignored files, submodules, nested repositories, file/directory transitions,
symlinks, awkward filenames, binary content, a repository with no commits, and a tree large enough
to fill a pipe buffer.

**If your change touches one of those invariants, or any code path that removes or overwrites a
path in the working tree, it needs a test that fails without the change.** Not a test that
exercises the code — a test that goes red when the fix is reverted.

Say in the pull request which test that is, and that you watched it fail. This is not ceremony:

- Four guards in this repository once survived being deleted with the suite still green, because
  the tests counted turns rather than asserting the property the guard existed for (`51d4d5f`).
- A round of nine fixes was checked by removing each one and re-running; two of the nine did not
  fail on the first attempt, because the fixtures could not reach the code being claimed —
  which is the same defect as the finding that prompted the check (`b282ba9`).

Assert the property, never the count.

Two more rules that come out of the same place:

- **Never spawn `git` outside [`src/git.rs`](src/git.rs), and never use porcelain.** Invariant 2 —
  a snapshot can never fire someone's `pre-commit` — is held by construction and by nothing else.
- **Never add a second implementation of `snap`, `plan` or `restore`.** The CLI, the recorder and
  the interface all call [`src/ops.rs`](src/ops.rs) so that a fix lands once.

## What a good pull request looks like here

- **One thing.** A reformat mixed with a behaviour change is two reviews and a merge conflict;
  `cargo fmt` was held back for a whole release cycle for exactly that reason (`2995a1d`).
- **The commit message explains the reasoning, not the diff.** `git log` in this repository is the
  best documentation it has: every non-obvious decision, and every reproduced data-loss path, is
  written down in the commit that fixed it. Match that. If the change is not obvious, say what the
  wrong version would have done.
- **English**, everywhere: code, comments, docs, commit messages, test names. Test names are
  sentences — `a_restore_that_fails_partway_puts_the_tree_back`.
- **Green on both platforms**, or say which one you could not run.
- **New dependency? Say why in the pull request**, and check it does not need a C toolchain. That
  constraint is why the turn log is NDJSON rather than SQLite and why `git` is a subprocess rather
  than libgit2.
- **Do not claim Windows.** Not in the manifest, the release matrix, CI or the docs, until
  `src/herdr/wire.rs` has a transport for it. `ef7acd5` explains what happened last time.

If you are working on the plugin half, remember that the dock and the recorder have to arrive at
the same timeline name from two different programs; [`tests/plugin_timeline.rs`](tests/plugin_timeline.rs)
runs both halves and compares them, and it is the test that will catch you.

## Reporting instead of fixing

A bug report is a real contribution, and for this project a good one is short. The two questions
that make it actionable are on the issue form: the output of `sheep doctor`, and whether the tree
was mid-operation. [`docs/known-limitations.md`](docs/known-limitations.md) lists what is already
understood and deliberately left — an issue about one of those is still welcome, it just starts
further along.

Anything that could cost somebody data goes to [`SECURITY.md`](SECURITY.md) instead of the issue
tracker.

## Licence

MIT. By contributing you agree your work is published under it.
