# AGENTS.md

Sheep is undo for AI coding agents: a [herdr](https://herdr.dev) plugin that turns every agent
turn into a restorable checkpoint, rewinds one worktree to before an agent broke things, and then
tells that agent what was taken back. `README.md` is the user-facing version.

This file is the canonical instructions for anything working in this repository, human or agent.
`CLAUDE.md` is a pointer to it and holds nothing of its own, so the two cannot drift.

**Read this before you touch `src/`.** Sheep overwrites files on other people's machines. One
credible "sheep ate my uncommitted work" report ends the project, so a good deal of this codebase
is not free to change, and the parts that are not are marked below.

---

## Build, test, check

```bash
cargo build                                             # target/debug/sheep
cargo test                                              # the whole suite; this is the gate
cargo clippy --all-targets -- -D warnings               # CI runs this on Linux and macOS
cargo fmt --check                                       # rustfmt.toml pins the style
shellcheck herdr-plugin/install.sh herdr-plugin/scripts/*.sh
```

All four are CI gates ([`.github/workflows/ci.yml`](.github/workflows/ci.yml)). Run at least
`cargo test` and `cargo clippy --all-targets -- -D warnings` before you claim a change works;
`-D warnings` fires on different code per platform, which is why CI runs both legs.

Rust 1.85, edition 2021, no `unsafe`, no build script. There is nothing to install beyond a
toolchain and `git` on `PATH`.

The plugin half has its own two scripts, run by CI and runnable by hand:

```bash
bash herdr-plugin/scripts/test-install.sh     # installer platform detection vs the release matrix
bash herdr-plugin/scripts/test-watchd.sh      # "exactly one recorder" under concurrent starts
```

`test-watchd.sh` refuses to run while a real recorder is alive, because the sweep it exercises
would take it.

### Always set `SHEEP_STATE_DIR` when you run the binary by hand

Sheep's state directory is real state on the machine you are on. Every manual run must redirect
it, or you will write turns into the developer's live timelines:

```bash
export SHEEP_STATE_DIR=$(mktemp -d)
cargo run -- doctor
cargo run -- snap --note "trying something"
```

Precedence is `HERDR_PLUGIN_STATE_DIR` > `SHEEP_STATE_DIR` > `XDG_STATE_HOME` >
`$HOME/.local/state/sheep` ([`src/repo.rs`](src/repo.rs), `state_dir_from`). `SHEEP_LINE` does the
same for `--line`.

---

## The safety invariants

These are the promises the product is. Each has a test written from the attacker's side — mostly
in [`tests/adversarial.rs`](tests/adversarial.rs) — except 2, which holds by construction.
**A change that touches one of these needs a test that fails without the change.**

1. **Never write into the user's `.git`.** Snapshots live in a separate bare repository whose
   `objects/info/alternates` points at the user's object database, so contents are borrowed rather
   than copied. Their index, HEAD, refs, stash and reflog are never modified.
   → `never_writes_into_the_users_git_directory`
2. **Plumbing only, never porcelain.** `add` / `write-tree` / `commit-tree` / `read-tree` /
   `checkout-index` do not run hooks, so a snapshot can never fire someone's `pre-commit`. Held by
   construction: [`src/git.rs`](src/git.rs) is the only place in the program that spawns `git`, and
   it strips eight hostile `GIT_*` variables from every child.
3. **Verify before touching anything.** A `git gc --prune` upstream can remove a borrowed object; a
   restore checks every object it will read first and refuses a partial tree.
   → `refuses_to_restore_a_snapshot_with_a_missing_object`
4. **Restore only the paths in the plan.** Three files means three files, and the plan a human read
   is the plan that runs — `ops::restore_expecting` refuses a tree that moved under it. A removal
   that is a directory on disk is refused outright: a repository nested in the worktree is one
   gitlink, so deleting it would take history no checkpoint could give back.
   → `a_restore_touches_only_the_paths_in_its_plan`,
   `a_restore_refuses_a_plan_the_tree_has_moved_out_from_under`,
   `a_nested_repository_is_never_deleted_by_a_restore`
5. **Undo must be undoable.** The state is checkpointed before a byte changes, and a restore that
   fails partway is put back from it. `ops::RestoreFailed` says which of the two happened.
   → `a_restore_is_itself_undoable`, `a_restore_that_fails_partway_puts_the_tree_back`
6. **Dry run is the default**; `--yes` is required on the command line, `shift+R` on a plan that is
   on screen.
7. **Refuse ambiguous state:** unmerged paths, mid-rebase/merge/cherry-pick/revert/bisect, non-git
   directories, worktrees over the file budget. Refusals, not warnings.
   → `refuses_a_worktree_with_unresolved_conflicts`, `refuses_a_worktree_mid_operation`,
   `refuses_a_directory_that_is_not_a_worktree`, `refuses_a_worktree_over_the_file_budget`
8. **Gitignored files are outside Sheep's reach in both directions** — never captured, never
   overwritten. This is what keeps `.env` and `node_modules/` safe. A file git *does* track is
   captured even when an ignore rule matches it; the two cases look alike and behave oppositely.
   → `gitignored_files_are_never_captured_and_never_removed`,
   `a_tracked_file_an_ignore_rule_matches_is_still_captured`,
   `a_genuinely_untracked_ignored_file_is_still_left_alone`

Invariant 4's second sentence generalises to the rule the whole write path is built on: **nothing
may be written over, or removed from, bytes no snapshot holds.** That is the shape all four of the
data-loss bugs in `89d28da` had.

---

## Where things are

| path | role |
|---|---|
| [`src/git.rs`](src/git.rs) | the only place that spawns `git`; scoped `--git-dir`/`--work-tree`, hostile env stripped |
| [`src/repo.rs`](src/repo.rs) | worktree discovery, safety guards (`Blocker` / `Warning`), state directory |
| [`src/shadow.rs`](src/shadow.rs) | the shadow repository: snapshot, verify, plan, apply, `rechain` |
| [`src/store.rs`](src/store.rs) | the turn log — append-only NDJSON, one file per timeline. `store::slug` maps a timeline name to something both a filename and a git ref accept, with a digest so two names cannot collide onto one ref |
| [`src/lock.rs`](src/lock.rs) | one advisory lock per **worktree**, not per timeline — all of a worktree's timelines share one shadow repository, and `gc --prune=now` on it deletes what another timeline has written but not yet pointed a ref at |
| [`src/ops.rs`](src/ops.rs) | what Sheep does: `snap`, `plan`, `restore`/`restore_expecting`, `collect`. The CLI, the recorder and the TUI all call this, never a parallel implementation |
| [`src/herdr/`](src/herdr) | `wire.rs` the socket protocol · `session.rs` the API slice, behind a trait · `detect.rs` a pure turn-boundary state machine over sightings and an explicit clock · `recorder.rs` joins them to `ops::snap` · `prompt.rs` screen-scraped prompts, never authoritative · `supervise.rs` reconnect policy · `log.rs` · `cli.rs` = `sheep watch` |
| [`src/tui/`](src/tui) | `app.rs` decides and never blocks — a restore is reachable only from a plan on screen · `engine.rs` the worker: plan, patch, restore, and the write-back that tells the agent · `render.rs` draws and reads nothing · `runtime.rs` the event loop behind traits, testable with no pty · `cli.rs` = `sheep ui` and `--snapshot` · `text.rs`/`theme.rs` |
| [`src/main.rs`](src/main.rs) | argument parsing only |
| [`herdr-plugin/`](herdr-plugin) | the herdr adapter: manifest, installer, recorder daemon, action shims |

[`docs/architecture.md`](docs/architecture.md) is the longer version: the shadow repository, turn
detection and the write-back, with the reasoning.

A **timeline** (`--line`) is one recording stream. `sheep watch` names one per agent per worktree
by default (`--line-by pane` for one per pane); a standalone `sheep snap` uses `default`.

### Commands

```bash
sheep doctor                # is this worktree safe to record
sheep snap                  # record the working tree as a turn
sheep log                   # the timeline
sheep diff '#3'             # what restoring turn 3 would do; touches nothing
sheep restore '#3' --yes    # go back
sheep gc --yes              # shorten history: rebuild the kept turns, then collect
sheep watch                 # the recorder; refuses outside a herdr session
sheep ui [--rewind]         # the dock, or the plan-and-restore overlay
sheep ui --snapshot 92x18   # one frame as plain text — reviewable in a PR, assertable in CI
```

Quote `#3`: an unquoted `#` starts a comment in every shell this runs in.

---

## Conventions that are real

- **English everywhere** in the repository: code, comments, docs, commit messages, test names.
- **No dependency that needs a C toolchain.** The binary must cross-compile to macOS and Linux
  without one, and to Windows the day that is claimed. That is why the turn log is NDJSON and not
  SQLite, and why `git` is a subprocess and not libgit2. Adding one is a decision, not a detail.
- **Linux and macOS are the only platforms anything here claims.** `wire::connect` has no non-unix
  arm, so on Windows every herdr call fails and `sheep watch` cannot record a turn; it refuses at
  startup instead of looping and exiting 0. The manifest declares `["linux", "macos"]`, no `.exe`
  is released, there is no Windows CI leg and there is no PowerShell surface — it is in git history
  and comes back with the transport. `cargo clippy --target x86_64-pc-windows-msvc --all-targets --
  -D warnings` is not a gate today; the one thing standing between the tree and a clean run is
  `wire::REQUEST_TIMEOUT` ([`src/herdr/wire.rs:29`](src/herdr/wire.rs#L29)), which wants
  `#[cfg(unix)]`.
- **Test names are sentences.** `a_restore_that_fails_partway_puts_the_tree_back`, not
  `test_restore_recovery`. The suite is meant to read as a list of promises.
- **Assert the property, never the count.** A test that counts turns is what hid four deleted
  guards (`51d4d5f`): filing a turn mid-write produces one turn too. Assert that the recorded tree
  is the tree on disk.
- **A comment explains why, not what.** The module docs in `src/lock.rs`, `src/git.rs` and
  `src/herdr/detect.rs` are the standard; they exist because every one of them encodes a decision
  someone would otherwise undo.

---

## Traps that have already cost someone a day

Every row is a bug that shipped or nearly shipped. The commit message explains it in full.

| trap | where | commit |
|---|---|---|
| Writing all of stdin before reading stdout **deadlocks** git's batch plumbing at the ~64 KB pipe buffer. Use `Git::run_stdin`, which streams on its own thread; read the comment before changing it. | [`src/git.rs:152`](src/git.rs#L152) | `72d39f8` |
| The scratch index starts **empty**, so every path looks untracked and git applies ignore rules to files the repository tracks. Tracked paths are staged explicitly — and only those still present as files, because force-adding a path that became a directory drags every ignored thing beneath it in. | [`src/shadow.rs`](src/shadow.rs) `write_tree` | `89d28da` |
| `checkout-index -f` **clears its own ground**: git's `remove_subtree` fires whenever a write target is a directory, so a one-line `write vendor` plan recursively deleted a nested repository. The guard is stated as the invariant, not as a list of shapes. | [`src/shadow.rs:548`](src/shadow.rs#L548) `apply` | `89d28da` |
| The shadow inherits the **machine's global** `core.autocrlf`. With `input` a CRLF file is recorded as LF and written back as LF, the checkpoint is normalised too, and `sheep snap` then says "nothing changed" — so Sheep cannot see the damage it did. `GIT_CONFIG_KEY_n` pins it off; in-tree `.gitattributes` is still honoured on purpose. | [`src/shadow.rs:157`](src/shadow.rs#L157) | `89d28da` |
| `apply` **removes before it writes**, and has to: a path changing between a file and a directory cannot be written while the old shape is there. A failure in the middle leaves a tree that is neither state, so the checkpoint is replayed automatically and `RestoreFailed` says which of the two happened. Never report "nothing was written" from that path. | [`src/ops.rs:220`](src/ops.rs#L220) | `fe7141f`, `b282ba9` |
| Recomputing the plan at the moment of the write means a user who agreed to three files can get nine. `restore_expecting` carries the tree the plan was computed against into the write. | [`src/ops.rs:287`](src/ops.rs#L287) | `92a3c1d` |
| A **bookkeeping** failure after a restore (a full state directory, a merge started in the second it took) is not a failed restore. Returning an error sends someone to undo something that worked. | [`src/ops.rs`](src/ops.rs) `Restored::bookkeeping_error` | `0b80a8a` |
| `collect` must read the **ref before the log**. The other order drops a turn appended between the two reads. `tests/collect_read_order.rs` makes a seam where two adjacent statements have none, and is a separate binary because the `PATH` it stubs is process-wide. | [`src/ops.rs:414`](src/ops.rs#L414) | `412a98e` |
| The lock is per **worktree**, not per timeline, and the loop must reach its deadline on every path — a `continue` on the stale-break path once pegged a core for ever. A lock stamped in the *future* is debris too, or a backwards clock step wedges a worktree permanently. | [`src/lock.rs`](src/lock.rs) | `b760793`, `412a98e` |
| `worktree_id` hashes the **path**, not just the directory name. Two linked worktrees both called `fix` is the ordinary shape of a herdr session; without the hash, `restore #1` in one writes the other's tree over it. | [`src/repo.rs`](src/repo.rs) `worktree_id` | `51d4d5f` |
| A herdr pane id contains a colon, and `refs/sheep/w31:pW` is refused by git outright. `store::slug` is the single mapping both callers share, and it carries a digest because the mapping is lossy — `w3:p1` and `w3/p1` would otherwise collapse onto one ref. | [`src/store.rs:255`](src/store.rs#L255) | `41fdaec` |
| **The dock and the recorder must name the same timeline.** `watchd.sh` starts `sheep watch` bare, so turns are filed under the agent; `sheep_target_line` reads `focused_pane_agent` out of herdr's invocation context to reach the same string, falling back to `default` — never to the pane id, which herdr reassigns on every restart. When they disagree the dock reads a timeline nothing writes and says "nothing recorded yet", which is indistinguishable from the truth. `tests/plugin_timeline.rs` runs both halves and compares them. | [`herdr-plugin/scripts/common.sh:82`](herdr-plugin/scripts/common.sh#L82) | `25f825a` |
| Herdr infers `working` from what a pane **paints**, so the edge arrives after the agent has begun. Baseline a pane on the first sighting, not on the turn edge, or the first real turn compares equal and goes unrecorded. | [`src/herdr/recorder.rs`](src/herdr/recorder.rs) | `2ee49eb` |
| A `cd` arriving mid-turn defeats every directory check made later — by then the move has been absorbed and each check compares the new directory against itself. `working_in` is fixed on the edge **into** `working`. | [`src/herdr/detect.rs`](src/herdr/detect.rs) | `f149564` |
| Mapping every server-reported error to "not there" makes one `invalid_request` drop every turn from then on, silently. Only `pane_not_found` and `agent_not_found` mean absence. A success envelope missing a key it promises is a fault, not an absence. | [`src/herdr/session.rs`](src/herdr/session.rs) `Live::optional` | `66451a7`, `f149564` |
| Clearing the read timeout **before** reading a subscription's acknowledgement leaves that read blocked with no deadline — verified against a listener that accepts and never writes, still blocked after 25 s. It sits upstream of every reconnect policy, so `sheep watch` wedged permanently instead of retrying. | [`src/herdr/wire.rs`](src/herdr/wire.rs) | `e9e570f` |
| Slicing a `String` by **byte** index panics mid-character, and the two places that show back a reply Sheep did not understand exist specifically to survive box-drawing output. An eight-byte slice of a short commit id took the whole interface down at any width above 62 columns. | [`src/herdr/wire.rs:37`](src/herdr/wire.rs#L37), [`src/tui/text.rs`](src/tui/text.rs) | `89d28da`, `c79ff1d` |
| Every `?` in the event loop was an exit that ignored an in-flight restore — possibly between `apply`'s deletions and its writes. A terminal that stops working is carried to the same exit `q` goes through. | [`src/tui/runtime.rs`](src/tui/runtime.rs) | `b55a8d2` |
| herdr 0.8 dispatches hooks **concurrently** — one session's log holds three overlapping `workspace.created` entries ten milliseconds apart — and both of Sheep's event hooks call `watchd.sh start`. Check-then-act on a pidfile left seven live recorders behind one pid. | [`herdr-plugin/scripts/watchd.sh`](herdr-plugin/scripts/watchd.sh) | `cadd0b1` |
| `watchd stop` must sweep only what `watchd start` started. `pgrep -u $(id -u) -f 'sheep watch'` is scoped to the user, so it killed the other herdr session's recorder too — and turns taken while a recorder is down are gone for good. | [`herdr-plugin/scripts/watchd.sh`](herdr-plugin/scripts/watchd.sh) | `078ebfd` |
| A relative **program** in a herdr `[[panes]]` entry resolves against the pane's own `--cwd`, which for Sheep is the agent's worktree, not the plugin root — and herdr never shell-expands argv. Panes launch `sh -c` and build an absolute path from `HERDR_PLUGIN_ROOT`. | [`herdr-plugin/herdr-plugin.toml`](herdr-plugin/herdr-plugin.toml) | `0bd7897` |

---

## What not to do without asking

- **Do not weaken a safety invariant**, or delete a guard because a test still passes without it.
  Four guards survived being deleted once (`51d4d5f`) because the tests counted instead of
  asserting. If you believe a guard is wrong, say so and leave it.
- **Do not add a dependency**, and especially not one that needs a C toolchain. See the conventions
  above; this is the reason the turn log is NDJSON.
- **Do not spawn `git` anywhere but [`src/git.rs`](src/git.rs)**, and do not reach for porcelain.
  Invariant 2 holds only because both of those are true.
- **Do not add a parallel implementation of `snap`, `plan` or `restore`.** The CLI, the recorder and
  the TUI all go through [`src/ops.rs`](src/ops.rs) so that a fix lands once.
- **Do not claim Windows**, in the manifest, the release matrix, CI or the docs, until
  `src/herdr/wire.rs` has a transport for it. `ef7acd5` un-claimed it everywhere at once and the
  reasoning is in that commit.
- **Do not change what the interface says about a restore** without reading `b282ba9` and
  `c79ff1d` first. Every sentence on those screens is load-bearing, and two of them once said things
  that were not true on exactly the frame where it mattered.
- **Do not run `sheep restore`, `sheep gc` or `sheep watch` against a real checkout** to try
  something out. Use a scratch repository and `SHEEP_STATE_DIR`.
- **Do not commit a reformat** mixed with anything else. `cargo fmt` was deliberately deferred until
  every branch had landed (`2995a1d`) because reformatting files three workers had open turns a
  whitespace pass into merge conflicts.

## Sending a change

[`CONTRIBUTING.md`](CONTRIBUTING.md) has the full version. The short one: a change that can write to
a user's files needs a test that fails without it, and you should say in the pull request which test
that is and that you watched it fail.
