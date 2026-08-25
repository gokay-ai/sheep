# Sheep — undo for AI coding agents

A [herdr](https://herdr.dev) plugin. An agent rewrites nine files in one turn and gets it wrong;
`git checkout .` throws away the last four good turns with it. Sheep makes every agent turn a
restorable checkpoint, rewinds one worktree to before it broke things, and tells that agent what
was taken back. `README.md` is the user-facing version of this.

Herdr is a terminal workspace manager for AI coding agents: persistent panes, per-pane agent
status (`idle`/`working`/`blocked`/`done`/`unknown`), a worktree per agent, and a unix-socket
JSON API. A plugin is a GitHub repo with a `herdr-plugin.toml` declaring actions, event hooks,
panes and keybindings; it runs as an argv command with `HERDR_SOCKET_PATH`, `HERDR_PLUGIN_ROOT`,
`HERDR_PLUGIN_STATE_DIR` and pane context in its environment.

## Safety invariants — these are not negotiable

Sheep overwrites files on other people's machines. One credible "sheep ate my uncommitted work"
report ends the project. Each has a test — mostly in `tests/adversarial.rs` — except 2, which
holds by construction: `src/git.rs` is the only place that spawns git, and it spawns plumbing.

1. **Never write into the user's `.git`.** Snapshots live in a separate bare repository whose
   `objects/info/alternates` points at the user's object database, so contents are borrowed
   rather than copied. Their index, HEAD, refs, stash and reflog are never modified.
2. **Plumbing only, never porcelain.** `add`/`write-tree`/`commit-tree`/`read-tree`/
   `checkout-index` do not run hooks, so a snapshot can never fire someone's `pre-commit`.
3. **Verify before touching anything.** A `git gc --prune` upstream can remove a borrowed
   object; a restore checks every object it will read first and refuses a partial tree.
4. **Restore only the paths in the plan.** Three files means three files, and the plan a human
   read is the plan that runs — `ops::restore_expecting` refuses a tree that moved under it. A
   removal that is a directory on disk is refused outright: a repository nested in the worktree
   is one gitlink, so deleting it would take history no checkpoint could give back.
5. **Undo must be undoable** — the state is checkpointed before a byte changes, and a restore
   that fails partway is put back from it. `ops::RestoreFailed` says which of the two happened.
6. **Dry run is the default**; `--yes` is required to write.
7. **Refuse ambiguous state:** unmerged paths, mid-rebase/merge/cherry-pick/revert/bisect,
   non-git directories, worktrees over the file budget.
8. **Gitignored files are outside Sheep's reach in both directions** — never captured, never
   overwritten. This is what keeps `.env` and `node_modules` safe.

## Layout

| path | role |
|---|---|
| `src/git.rs` | the only place that spawns `git`; scoped `--git-dir`/`--work-tree`, hostile env stripped |
| `src/repo.rs` | worktree discovery, safety guards (`Blocker` / `Warning`), state directory |
| `src/shadow.rs` | the shadow repository: snapshot, verify, plan, apply, `rechain` |
| `src/store.rs` | the turn log — append-only NDJSON, one file per timeline. `store::slug` maps a timeline name to something both a filename and a git ref accept, with a digest so two names cannot collide onto one ref |
| `src/ops.rs` | what Sheep does: `snap`, `plan`, `restore`/`restore_expecting`, `collect`. The CLI, the recorder and the TUI all call this, never a parallel implementation |
| `src/herdr/` | `wire.rs` the socket protocol · `session.rs` the API slice, behind a trait · `detect.rs` a pure turn-boundary state machine over sightings and an explicit clock · `recorder.rs` joins them to `ops::snap` · `prompt.rs` screen-scraped prompts, never authoritative · `supervise.rs` reconnect policy · `log.rs` · `cli.rs` = `sheep watch` |
| `src/tui/` | `app.rs` decides and never blocks — a restore is reachable only from a plan on screen · `engine.rs` the worker: plan, patch, restore, and the write-back that tells the agent · `render.rs` draws and reads nothing · `runtime.rs` the event loop behind traits, testable with no pty · `cli.rs` = `sheep ui` and `--snapshot` · `text.rs`/`theme.rs` |
| `src/main.rs` | argument parsing only |

A **timeline** (`--line`) is one recording stream. `sheep watch` names one per agent per worktree
by default (`--line-by pane` for one per pane); a standalone `sheep snap` uses `default`.

**The plugin has to name timelines the same way the recorder does.** `watchd.sh` starts
`sheep watch` with no flags, so turns are filed under the agent — `claude`, `codex` — and
`herdr-plugin/scripts/common.sh:sheep_target_line` reads `focused_pane_agent` out of herdr's
invocation context to reach the same string, falling back to `default` (never to the pane id,
which herdr reassigns on every restart). When the two disagree the dock reads a timeline nothing
writes and says "nothing recorded yet", which is indistinguishable from the truth — so
`tests/plugin_timeline.rs` runs both halves and compares them, and the dock's empty state names
the other timelines `Store::lines_for` can see.

## Commands

```bash
cargo test                  # the adversarial suite is the gate — it must stay green
cargo clippy --all-targets -- -D warnings && cargo fmt --check

sheep doctor                # is this worktree safe to record
sheep snap                  # record the working tree as a turn
sheep log                   # the timeline
sheep diff #3               # what restoring turn 3 would do; touches nothing
sheep restore #3 --yes      # go back
sheep gc --yes              # shorten history: rebuild the kept turns, then collect
sheep watch                 # the recorder; needs a herdr session
sheep ui [--rewind]         # the dock, or the plan-and-restore overlay
sheep ui --snapshot 92x18   # one frame as plain text — reviewable in a PR, assertable in CI
```

`SHEEP_STATE_DIR` overrides the state directory — always set it when testing so nothing lands
in the real one. Precedence: `HERDR_PLUGIN_STATE_DIR` > `SHEEP_STATE_DIR` > `XDG_STATE_HOME`.
`SHEEP_LINE` does the same for `--line`.

## Conventions

- **English** everywhere in the repository: code, comments, docs, commit messages.
- No dependency that needs a C toolchain — the binary must cross-compile to macOS, Linux and
  Windows without one. That is why the turn log is NDJSON and git is a subprocess, not libgit2.
- Git's batch plumbing deadlocks if you write all of stdin before reading stdout. Use
  `Git::run_stdin`, which streams on its own thread; read the comment there before changing it.
