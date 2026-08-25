<h1 align="center">Sheep</h1>

<p align="center"><strong>Undo for AI coding agents. Every agent turn becomes a restorable checkpoint.</strong></p>

<p align="center">
  <a href="https://github.com/gokay-ai/sheep/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/gokay-ai/sheep/actions/workflows/ci.yml/badge.svg"></a>
  <a href="https://github.com/gokay-ai/sheep/releases/latest"><img alt="latest release" src="https://img.shields.io/github/v/release/gokay-ai/sheep?sort=semver"></a>
  <img alt="MIT licence" src="https://img.shields.io/badge/licence-MIT-blue">
</p>

```text
 sheep  sheep                                                                          ready
timeline claude · 5 turns · newest 12s ago · notify on
╭ timeline ────────────────────────────────────────────────────────────────────────────────╮
│▌#5   manual     claude                                                 637b1b65 · 12s ago│
│▌  10 files   +18 −87   ████████████                                                      │
│▌  extract the retry helper and use it everywhere                                         │
│▌  637b1b6519de · 2026-08-25 19:13 UTC                                                    │
│ #4   manual     claude                                                  75896f9e · 1m ago│
│   1 file   +3 −0   █                                                                     │
│   add a theme helper                                                                     │
│ #3   manual     claude                                                  2d7c34cb · 4m ago│
│   2 files   +5 −0   █                                                                    │
│   cap the note length                                                                    │
│                                                                                          │
╰───────────────────────────────────────────────────────────────────────────────────── 1/5 ╯
j/k move · enter rewind · ? keys · q quit · n notify · r refresh
```

Sheep is a plugin for [herdr](https://herdr.dev), the terminal workspace manager that runs your coding agents in panes and hands each one its own git worktree. It watches herdr's per-pane agent status, snapshots the worktree the moment an agent finishes a turn, and keeps every snapshot in a git object store beside your repository rather than inside it.

```bash
herdr plugin install gokay-ai/sheep/herdr-plugin
```

No Rust needed: the install step downloads a checksum-verified binary for your platform, registers the timeline dock, the rewind overlay and the recorder, and starts recording. Herdr 0.8 reads keybindings from your own config, so paste [`herdr-plugin/keybindings.toml`](herdr-plugin/keybindings.toml) into `~/.config/herdr/config.toml` to get `prefix+Z` for the dock and `prefix+z` for the rewind overlay; until then, `herdr plugin action invoke dock --plugin sheep` and `… invoke rewind --plugin sheep` do the same thing.

## "Claude Code already has `/rewind`"

It does, and if you run one agent, in one session, on one checkout — use it. Sheep exists for the setup where that stops being enough.

- **It is cross-harness.** Sheep reads herdr's per-pane agent status, not any one vendor's transcript, so `claude`, `codex`, `opencode`, `grok` and everything else herdr attributes an agent to are recorded the same way, in the same format — one timeline per agent per checkout.
- **It outlives the session.** `/rewind` lives inside one conversation in one CLI, and it goes when that goes. A Sheep timeline is an append-only NDJSON log plus a bare git repository under your state directory: it is still there tomorrow, `sheep log` reads it with no agent running at all, and the agent that made it does not have to still exist.
- **It is per worktree, across parallel agents.** Four agents in four worktrees are four timelines, keyed by the worktree and by the agent. A conversation-scoped undo cannot express "put *that* agent's checkout back, and only that one" at all.
- **It tells the agent what you took back.** After a restore, Sheep sends the agent a message through herdr's `agent.prompt`: which turn, how many paths were rewritten and deleted, that anything written after that turn is gone from disk, that it must re-read before editing, and the checkpoint that undoes the undo. This is the part nothing else here does, and it is the difference between an agent that re-reads and one that cheerfully re-applies the edit you just reverted.

Sheep does not restore a conversation. It restores files, and then tells the conversation.

## What it does

### Records, without being asked

`sheep watch` holds one subscription to herdr and follows every agent pane in the session. When a pane leaves `working` for `idle`/`done` it opens a *candidate* boundary — and then refuses to believe it for ten seconds, because herdr infers status from what a pane paints and calls agents `done` mid-turn. The default settle window is measured rather than guessed: a live herdr 0.8.0 session was watched flipping a pane to `done` and back to `working` **9.2 seconds later**, with the agent still working.

It says what it did in a log file rather than in the pane you are using. From a real run against
a session with eight agent panes (home paths shortened):

```text
$ tail -f ~/.local/state/herdr/plugins/sheep/logs/watch.log
2026-08-25 18:54:43Z info w3K:p1: baseline #1 on w3K:p1 — 57 file(s) in ~/…/herdr-max
2026-08-25 18:56:45Z info w3S:p1: herdr no longer lists an agent here; forgetting it
2026-08-25 18:59:26Z info w3N:p1: still spawning processes; waiting
2026-08-25 18:59:36Z info w3N:p1: nothing changed on w3N:p1; not recorded
2026-08-25 19:02:58Z info w31:pW: recorded #2 on w31:pW — 5 file(s) +290 -162 in ~/…
```

An agent that answered a question without editing anything does not get a row. Neither does a
candidate that goes back to `working`, or to `blocked`, or whose pane changes directory while the
turn is in flight, or whose foreground process group is still starting things — each of those
withdraws the boundary and says which one it was.

### Shows you the plan before it writes anything

`sheep ui --rewind`, or `prefix+z`. Pick a turn; every path it would touch is split into what gets written and what gets removed, the diff for the selected file is read out of the snapshot, and the footer says the consequence in words. Restore is `shift+R` and nothing else — `enter` opens a diff, lower-case `r` is refresh, and a plan nobody has looked at cannot be applied.

```text
 sheep  sheep                                                                          ready
timeline claude · 5 turns · newest 12s ago · notify on
╭ rewind to #4 ────────────────────────────────────────────────────────────────────────────╮
│back to #4  1m ago · claude · manual                                          75896f9e7a2f│
│add a theme helper                                                                        │
│10 paths change  —  10 written · 0 removed                                                │
│──────────────────────────────────────────────────────────────────────────────────────────│
│ will be written (10)                 ╭ src/git.rs ──────────────────────────────────────╮│
│▌+ src/git.rs                         │@@ -192,5 +192,3 @@ impl Git {                    ││
│ + src/herdr/detect.rs                │ pub fn canonical(path: &Path) -> Result<PathBuf> ││
│ + src/ops.rs                         │     std::fs::canonicalize(path).with_context(|| f││
│ + src/repo.rs                        │ }                                                ││
│ + src/shadow.rs                      │-                                                 ││
│ + src/store.rs                       │-// refactor: routed through the new abstraction  ││
│ + src/tui/app.rs                     │                                                  ││
│ + src/tui/engine.rs                  │                                                  ││
│ + src/tui/render.rs                  │                                                  ││
│ + src/tui/theme.rs                   │                                                  ││
│                                      │                                                  ││
│                                      │                                                  ││
│                                      │                                                  ││
│                                      │                                                  ││
│                                      │                                                  ││
│                                      ╰──────────────────────────────────────────────────╯│
│──────────────────────────────────────────────────────────────────────────────────────────│
│ restoring rewrites 10 files and deletes 0 files under sheep/.                            │
│ the tree you have now is snapshotted first as a new turn, so this is undoable.           │
│ the agent in pane w3K:p1 will be told what was taken back.                               │
│ shift+R  restore   esc back · d diff · J/K scroll · n notify · q quit                    │
╰──────────────────────────────────────────────────────── dry run — nothing is written yet ╯
```

Both frames above are real output: `sheep ui --snapshot 92x16` and `sheep ui --rewind --select 4 --keys d --snapshot 92x30`, run against a five-turn timeline. `--snapshot` draws one frame of the interface into an off-screen buffer and writes it to stdout as text, so what you are looking at can be reviewed in a pull request and asserted in CI with no pseudo-terminal involved. Those five turns were recorded by hand with `sheep snap`, which is why they are labelled `manual` — the recorder labels its own `turn`.

The same plan on the command line, which touches nothing either:

```console
$ sheep diff '#4'
restore to 75896f9e7a2f  ·  10 file(s) written, 0 removed
  write   src/git.rs
  write   src/herdr/detect.rs
  write   src/ops.rs
  write   src/repo.rs
  write   src/shadow.rs
  write   src/store.rs
  write   src/tui/app.rs
  write   src/tui/engine.rs
  write   src/tui/render.rs
  write   src/tui/theme.rs
```

### Tells the agent what you took back

The message that lands in the agent's pane after a restore:

> `[sheep]` Your working tree was rewound to turn #4 (75896f9e7a2f). 10 path(s) changed on disk: 10 rewritten, 0 deleted. Anything you wrote after turn #4 is no longer on disk — re-read any file before you edit it, and do not re-apply the reverted changes unless you are asked to. The state from just before the rewind was kept as turn #6; `sheep restore #6 --yes` puts it back.

Outside herdr this is a no-op by construction rather than by a branch, so the interface behaves identically in a plain terminal.

## What Sheep never touches

Sheep overwrites files on other people's machines. Everything below is a promise with a test behind it, in [`tests/adversarial.rs`](tests/adversarial.rs) — 31 tests written from the attacker's side, part of a suite of 180.

**Your `.git`.** Snapshots live in a separate bare repository under Sheep's state directory whose `objects/info/alternates` points at your object database, so unchanged content is *borrowed*, never copied. Your index, HEAD, branches, stash and reflog are never written. Uninstalling is `rm -rf` on one directory. → `never_writes_into_the_users_git_directory`

**Your hooks.** Only git plumbing is used — `add`, `write-tree`, `commit-tree`, `read-tree`, `checkout-index` — none of which runs a repository hook. A snapshot can never fire your `pre-commit`. (Held by construction: `src/git.rs` is the only place in the program that spawns git, and it strips `GIT_DIR`, `GIT_WORK_TREE`, `GIT_INDEX_FILE` and five more from every child.)

**Anything `.gitignore`d.** `git add -A` honours your ignore rules, so `.env`, `node_modules/` and build output are outside Sheep's reach in both directions: never captured, and therefore never overwritten. `sheep doctor` says so out loud. → `gitignored_files_are_never_captured_and_never_removed`

**Any path not in the plan.** A three-file restore rewrites three files. → `a_restore_touches_only_the_paths_in_its_plan`

**A repository nested inside your worktree.** `git add -A` records one as a single gitlink — a commit pointer and nothing else — so a restore to before it appeared produces a one-line plan, `remove vendor/parser`, whose contents no snapshot holds. Deleting it would take that repository's history, its uncommitted work and its ignored files with it, and the checkpoint could not bring any of it back. Sheep refuses before touching anything:

```console
$ sheep restore '#1' --yes
restore to f8223a8c6bb2  ·  0 file(s) written, 1 removed
  remove  vendor/parser
sheep: the restore failed: refusing to restore: `vendor/parser` is a directory whose contents
Sheep never captured. A git repository inside your worktree is recorded only as a pointer, so
removing it here would delete files no snapshot holds — including anything ignored inside it.
Move or delete it yourself if that is what you want.. Your files were put back as they were.
```

→ `a_nested_repository_is_never_deleted_by_a_restore`

**A tree it cannot vouch for.** Unmerged paths, a rebase/merge/cherry-pick/revert/bisect in flight, a directory that is not a worktree, a checkout over the file budget: all refusals, not warnings. → `refuses_a_worktree_with_unresolved_conflicts`, `refuses_a_worktree_mid_operation`, `refuses_a_directory_that_is_not_a_worktree`, `refuses_a_worktree_over_the_file_budget`

**A snapshot with a hole in it.** Because objects are borrowed, an aggressive `git gc --prune=now` in your repository could in principle remove one a snapshot still references. A restore checks every object it is about to read, first, and refuses rather than leaving a half-restored tree. → `refuses_to_restore_a_snapshot_with_a_missing_object`

**The state you are in now.** It is checkpointed as a turn before a single byte changes, so the undo is itself undoable. If a restore fails partway — it removes before it writes, because a path that changes between a file and a directory cannot be written while the old shape is there — the checkpoint is exactly what was on disk a moment ago, so Sheep replays it automatically and then tells you which of the two things happened. → `a_restore_is_itself_undoable`, `a_restore_that_fails_partway_puts_the_tree_back`

**The plan you read.** An agent keeps working while you are reading a plan. Sheep carries the tree the plan was computed against into the write and refuses if it moved, handing back the plan as it stands now instead of applying one nobody looked at. → `a_restore_refuses_a_plan_the_tree_has_moved_out_from_under`

And nothing is written without an explicit confirmation: `--yes` on the command line, `shift+R` on a plan that is on the screen in front of you.

## How it works

**The shadow repository.** One bare git repository per worktree, under Sheep's state directory, with `objects/info/alternates` pointing at your object database (and at anything your repository itself borrows from, so a `--reference` clone resolves too). A snapshot is `git add -A` into a throwaway index, `write-tree`, `commit-tree`, `update-ref refs/sheep/<timeline>` — your index is never involved. Anything your repository already holds as an object costs nothing to snapshot; only what has changed since your last commit is written, and only once.

**Turn detection at corroborated status edges.** A candidate opens only on `working` → at-rest, and only for a pane that has actually been seen working since its last recorded turn. It has to survive a quiet window with no status change and no new output — herdr's per-pane revision counter is what "no new output" means, and a still-working agent paints. Going back to `working`, `blocked` or `unknown` withdraws it. So does moving directory: a turn is bound to the directory it *started* in, fixed on the edge into `working`, because a `cd` that arrives mid-turn is otherwise absorbed by every later check. When the window elapses, the recorder corroborates before writing: it re-asks herdr for the pane, and it asks the kernel — if the pids in the pane's foreground process group changed, that is an agent running tools, not an agent that has stopped, and the window is extended.

**The write-back.** A restore driven from the overlay finishes by sending the agent in that pane a message over herdr's `agent.prompt`. The turn number, the file counts, the instruction to re-read, and the checkpoint that reverses it.

**Costs, measured.** On a 12,000-file, 95 MB checkout: first snapshot 1.17 s, subsequent snapshots 0.35 s, `sheep diff` 0.32 s, a restore 1.43 s. Recording a whole live herdr session — eight agent panes across six checkouts — cost 1.5 MB of state; the two largest of those checkouts are 1.05 GB between them and account for 440 KB of it, because unchanged content is borrowed rather than copied. `sheep gc --keep 10` took a 32-turn timeline's shadow repository from 315 KB to 131 KB, and turn #28 restored to a byte-identical tree before and after — which is what `a_kept_turn_still_restores_to_the_same_files_after_collection` asserts in general.

## Commands and configuration

```console
$ sheep -h
Undo for AI coding agents.

Usage: sheep [OPTIONS] <COMMAND>

Commands:
  snap     Record the working tree as a new turn
  log      List recorded turns, newest first
  diff     Show what restoring a turn would do. Never writes anything
  restore  Restore the working tree to a turn. Dry run unless --yes is given
  doctor   Report whether this worktree is safe for Sheep to record and restore
  gc       Shorten recorded history to what the retention policy allows
  watch    Watch the herdr session and record a turn whenever an agent finishes one
  ui       Open Sheep's terminal interface: the timeline dock, or the rewind picker
  help     Print this message or the help of the given subcommand(s)

Options:
  -C, --repo <REPO>            Worktree to operate on. Defaults to the current directory
      --line <LINE>            Timeline to record against. One per agent pane; `default` when standalone [env: SHEEP_LINE=] [default: default]
      --max-files <MAX_FILES>  Refuse to touch a worktree with more tracked files than this [default: 60000]
  -h, --help                   Print help (see more with '--help')
  -V, --version                Print version
```

```console
$ sheep log
#5    637b1b6519de  manual       10 files  +18     -87     extract the retry helper and use it everywhere
#4    75896f9e7a2f  manual        1 files  +3      -0      add a theme helper
#3    2d7c34cb256c  manual        2 files  +5      -0      cap the note length
#2    a2e6dd9adcc3  manual        1 files  +2      -0      raise the settle window to 12s
#1    a2833b233722  manual       58 files  +0      -0      start of session
```

`sheep gc` is the one worth knowing: trimming the turn log alone frees nothing, because every old commit stays reachable through the parent chain, so `gc` rebuilds the kept turns as a fresh chain against the same trees — every one of them still restores to exactly the same bytes — and only then collects. `--keep 500` and `--max-age-days 30` by default, dry run unless `--yes`.

### Two things you would otherwise never find

**Put the turn number in herdr's sidebar.** After every recorded turn, Sheep reports `turn = "#<n>"` to herdr as pane metadata with a four-hour TTL. Herdr renders custom pane metadata as a `$name` token, so one row in `~/.config/herdr/config.toml` puts each agent's current turn beside its name:

```toml
[ui.sidebar.agents]
rows = [["state_icon", "workspace", "tab"], ["agent", "$turn"]]
```

Then `herdr server reload-config`. Without that row the metadata is reported and never displayed.

**`SHEEP_LINE` picks the timeline a pane belongs to.** A pane's command line is fixed by the plugin manifest, so two docks launched from the same manifest entry are the same argv; the environment the pane is opened with is the only thing that can tell one from another. `SHEEP_LINE` is read as the default for `--line`, which is how a dock knows whose timeline it is showing.

Other environment: `SHEEP_STATE_DIR` overrides where state lives, with precedence `HERDR_PLUGIN_STATE_DIR` > `SHEEP_STATE_DIR` > `XDG_STATE_HOME` > `~/.local/state/sheep`. Set it when you are experimenting so nothing lands in the real one.

Keybindings live in your herdr config rather than in the plugin manifest — herdr 0.8 reads the key map from `config.toml` only. [`herdr-plugin/keybindings.toml`](herdr-plugin/keybindings.toml) is the two bindings ready to paste, Windows action ids included.

### Building it yourself

```bash
cargo build --release        # target/release/sheep
cargo test                   # 180 tests
cargo clippy --all-targets -- -D warnings && cargo fmt --check
```

CI runs fmt, clippy and the tests on Linux, macOS and Windows; releases are built natively for six targets with one `SHA256SUMS`.

## Prior art, and thanks

[**herdr**](https://herdr.dev) is the reason any of this is possible. The per-pane agent status, the worktree-per-agent model and the socket API are what Sheep is built out of; without them a plugin cannot know that an agent finished a turn, or which checkout it finished it in.

**Claude Code's `/rewind`** is the right idea and is where most people will meet it first. If it covers your setup, it is fewer moving parts than this.

**Cursor** keeps per-message checkpoints inside the editor, and **aider** commits every change it makes straight to git, which is the oldest and bluntest version of this idea and still a good one. **Jujutsu** (`jj op restore`) goes further than either by making the whole operation log undoable — if you already work in `jj`, you have most of what Sheep offers for one repository at a time.

Sheep's contribution is the awkward middle: several agents, several worktrees, several harnesses, one machine — and telling the agent what changed underneath it.

---

MIT licensed. Snapshots are yours; `rm -rf` on the state directory removes every trace.
