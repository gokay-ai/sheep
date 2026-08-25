# Sheep — undo for AI coding agents

A [herdr](https://herdr.dev) plugin. An agent rewrites nine files in one turn and gets it
wrong; `git checkout .` throws away the last four good turns with it. Sheep makes every agent
turn a restorable checkpoint, rewinds one agent's worktree to before it broke things, and tells
that agent what was taken back.

Herdr is a terminal workspace manager for AI coding agents: persistent panes, per-pane agent
status (`idle` / `working` / `blocked` / `done` / `unknown`), git-worktree-per-agent workflows,
and a local unix-socket JSON API. A plugin is a public GitHub repo with a `herdr-plugin.toml`
manifest; it can declare actions, event hooks, panes, link handlers and keybindings, and runs
as an ordinary argv command with `HERDR_SOCKET_PATH`, `HERDR_BIN_PATH`,
`HERDR_PLUGIN_STATE_DIR`, `HERDR_PLUGIN_CONFIG_DIR` and pane/workspace context in its
environment.

## Safety invariants — these are not negotiable

Sheep overwrites files on other people's machines. One credible "sheep ate my uncommitted work"
report ends the project. Every one of these is enforced by a test in `tests/adversarial.rs`:

1. **Never write into the user's `.git`.** Snapshots live in a separate bare repository under
   the state directory whose `objects/info/alternates` points at the user's object database, so
   contents are borrowed rather than copied. The user's index, HEAD, refs, stash and reflog are
   never modified.
2. **Plumbing only, never porcelain.** `add` / `write-tree` / `commit-tree` / `read-tree` /
   `checkout-index` do not run repository hooks, so a snapshot can never fire someone's
   `pre-commit`.
3. **Verify before touching anything.** Borrowed objects can in principle be pruned by a
   `git gc` in the user's repo; a restore checks every object it will read first and refuses
   rather than leaving a half-restored tree.
4. **Restore only the paths in the plan.** A three-file restore rewrites three files.
5. **Undo must be undoable** — the current state is checkpointed before a byte changes.
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
| `src/shadow.rs` | the shadow repository: snapshot, verify, plan, apply |
| `src/store.rs` | the turn log — append-only NDJSON, one file per timeline |
| `src/ops.rs` | what Sheep does: `snap`, `plan`, `restore`. The CLI, the recorder and the TUI all call this, never a parallel implementation |
| `src/herdr/` | the herdr socket client and turn-boundary detection |
| `src/tui/` | the timeline dock and the rewind overlay |
| `src/main.rs` | argument parsing only |

A **timeline** (`--line`) is one recording stream. Under herdr there is one per agent pane; a
standalone `sheep snap` uses `default`.

## Commands

```bash
cargo build                 # debug
cargo test                  # the adversarial suite is the gate — it must stay green
cargo clippy --all-targets -- -D warnings
cargo fmt --check

sheep doctor                # is this worktree safe to record
sheep snap                  # record the working tree as a turn
sheep log                   # the timeline
sheep diff #3               # what restoring turn 3 would do; touches nothing
sheep restore #3 --yes      # go back
```

`SHEEP_STATE_DIR` overrides the state directory — always set it when testing so nothing lands
in the real one. Precedence: `HERDR_PLUGIN_STATE_DIR` > `SHEEP_STATE_DIR` > `XDG_STATE_HOME`.

## Conventions

- **English** everywhere in the repository: code, comments, docs, commit messages.
- No dependency that needs a C toolchain — the binary must cross-compile to macOS, Linux and
  Windows without one. That is why the turn log is NDJSON rather than SQLite and why git is
  driven as a subprocess rather than through libgit2.
- Prefer git plumbing over porcelain; see invariant 2.
- Git's batch plumbing (`cat-file --batch-check`, `checkout-index --stdin`) **deadlocks** if you
  write all of stdin before reading stdout — the child blocks once its ~64 KB stdout buffer
  fills. `Git::run_stdin` streams stdin on its own thread; keep it that way.
