<h1 align="center">Sheep</h1>

<p align="center"><strong>Undo for AI coding agents. Every agent turn becomes a restorable checkpoint.</strong></p>

<p align="center">
  <a href="https://github.com/gokay-ai/sheep/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/gokay-ai/sheep/actions/workflows/ci.yml/badge.svg"></a>
  <a href="https://github.com/gokay-ai/sheep/releases/latest"><img alt="latest release" src="https://img.shields.io/github/v/release/gokay-ai/sheep?sort=semver"></a>
  <img alt="MIT licence" src="https://img.shields.io/badge/licence-MIT-blue">
</p>

[herdr](https://herdr.dev) runs your coding agents in panes, a git worktree each. That is the herd.
Sheep is what you reach for when one of them wanders off.

Not the whole flock rounded up, and not the field burned down and reseeded — that one animal, walked
back to where it was, while the rest carry on grazing.

An agent rewrites ten files in one turn and gets three of them wrong. `git checkout .` is the burnt
field: it takes the last four good turns along with the bad one, and it cannot tell the agent
anything. Sheep puts **that** worktree back to a turn you pick, leaves every other agent exactly
where it was, and then tells the agent what was taken back — so it re-reads the files instead of
cheerfully re-applying the edit you just reverted.

```bash
herdr plugin install gokay-ai/sheep/herdr-plugin
```

No Rust needed: the install step fetches a checksum-verified binary for your platform, registers the
timeline dock, the rewind overlay and the recorder, and starts recording. Linux and macOS —
[a claim, not an oversight](#linux-and-macos-only). The command-line half works in any git checkout
with no herdr at all; the plugin is what makes it automatic.

```text
 sheep  sheep                                                                          ready
timeline claude · 5 turns · newest 43s ago · notify on
╭ timeline ────────────────────────────────────────────────────────────────────────────────╮
│▌#5   manual     claude                                                 90f73711 · 43s ago│
│▌  10 files   +10 −75   ████████████                                                      │
│▌  extract the retry helper and use it everywhere                                         │
│▌  90f73711129f · 2026-08-26 07:42 UTC                                                    │
│ #4   manual     claude                                                  f944f1ef · 1m ago│
│   1 file   +1 −1   █                                                                     │
│   lower the reconnect ceiling to 20s                                                     │
│ #3   manual     claude                                                  f0154c28 · 3m ago│
│   2 files   +3 −0   █                                                                    │
│   add a theme helper                                                                     │
│ #2   manual     claude                                                  19540f92 · 5m ago│
│   1 file   +2 −0   █                                                                     │
│   cap the note length                                                                    │
╰───────────────────────────────────────────────────────────────────────────────────── 1/5 ╯
j/k move · enter rewind · ? keys · q quit · n notify · r refresh
```

<sup>`sheep ui --snapshot 92x18`. Every frame here is real output: `--snapshot` draws one frame of the
interface into an off-screen buffer and prints it as text, so what you are looking at is reviewable
in a pull request and assertable in CI —
[`docs/readme/make-frames.sh`](docs/readme/make-frames.sh) builds the fixture and takes both
photographs. These five turns were recorded by hand with `sheep snap`, which is why they read
`manual`; the recorder labels its own `turn`.</sup>

## "Claude Code already has `/rewind`"

It does, it is good, and if you run one agent in one session on one checkout, use it — fewer moving
parts than this. Sheep is for the setup where that stops being enough.

- **It is cross-harness.** Sheep reads herdr's per-pane agent status, not one vendor's transcript
  format, so `claude`, `codex`, `opencode`, `gemini`, `grok` — everything in herdr's detection table —
  is recorded identically, one timeline per agent per checkout.
- **It outlives the session.** `/rewind` lives inside one conversation in one CLI and goes when that
  goes. A Sheep timeline is an append-only NDJSON log plus a bare git repository in your state
  directory: `sheep log` reads it tomorrow, with no agent running and no conversation left alive.
- **It is per worktree, across parallel agents.** Four agents in four worktrees are four timelines,
  keyed by worktree and by agent. A conversation-scoped undo cannot express "put *that* agent's
  checkout back, and only that one" at all.
- **It tells the agent what you took back.** After a restore, Sheep sends that pane's agent a message
  through herdr's `agent.prompt`: which turn, how many paths were rewritten and deleted, that
  anything written after that turn is gone from disk, that it must re-read before editing, and the
  checkpoint that undoes the undo.

Sheep does not restore a conversation. It restores files, and then tells the conversation.

## What it does

### It records without being asked

`sheep watch` holds one subscription to herdr and follows every agent pane in the session. When a
pane leaves `working` for `idle`/`done` it opens a *candidate* boundary — and then declines to
believe it for ten seconds, because herdr infers status from what a pane paints and will call an
agent `done` in the middle of a turn.

That is not a hypothetical, and the ten seconds is measured rather than guessed. It says what it did
in a log file rather than in the pane you are using. Thirteen minutes of one, from a live herdr
session with ten agent panes in it — started with `--line-by pane`, so the timelines are named after
panes rather than after agents, and with home paths shortened:

```console
$ tail -f ~/.local/state/herdr/plugins/sheep/logs/watch.log
2026-08-26 07:39:49Z info watching: settle 10.0s, patience 120s, timelines by Pane, log …/logs/watch.log
2026-08-26 07:39:51Z info w3K:p1: baseline #1 on w3K:p1 — 60 file(s) in ~/…/herdr-max
2026-08-26 07:39:51Z info w3K:p0: baseline #1 on w3K:p0 — 60 file(s) in ~/…/herdr-max-projectdocs
2026-08-26 07:39:51Z info w3K:pZ: baseline #1 on w3K:pZ — 60 file(s) in ~/…/herdr-max-readme
2026-08-26 07:40:27Z info w3K:p1: withdrawn — the pane went back to working — false done
2026-08-26 07:41:33Z info w20:p3N: nothing changed on w20:p3N; not recorded
2026-08-26 07:52:42Z info w3K:p0: still spawning processes; waiting
2026-08-26 07:52:52Z info w3K:p0: recorded #2 on w3K:p0 — 11 file(s) +1100 -97 in ~/…/herdr-max-projectdocs
```

Every line there is a decision. The first thing the recorder does with a pane is take a baseline,
because the state *before* an agent's first turn is the one you most want back and no boundary
announces it. Thirty-six seconds in, herdr calls `w3K:p1` done, the recorder waits, and the pane goes
back to `working` — that is the false `done`, and withdrawing is the whole defence against it. An
agent that answered a question without editing anything gets no row either. And the last two lines
are one turn: herdr said the pane was at rest, the kernel said its process group was still starting
things, so the window was extended by ten seconds and the turn recorded after that.

`blocked` withdraws a candidate too — an agent waiting on you has not finished a turn — and so does a
pane that changed directory mid-turn, because the tree that would be snapshotted is no longer the
tree the agent worked in.

That run left this behind, which is a whole timeline recorded without anybody asking for one:

```console
$ sheep log --line w3K:p0
#2    8d615da1a2eb  turn         11 files  +1100   -97
#1    c5e3e6f87356  checkpoint   60 files  +0      -0      baseline, before the first recorded turn
```

The note on `#2` is empty on purpose. No API tells a plugin what a user typed, so the prompt beside a
turn is read off the pane — and when it has scrolled away Sheep leaves the column blank rather than
filling it with something that is nearly right.

### It shows you the plan before it writes anything

`sheep ui --rewind`, or `prefix+z`. Pick a turn: every path it would touch is split into what gets
written and what gets removed, the diff for the selected file is read out of the snapshot, and the
footer says the consequence in words. Restore is `shift+R` and nothing else — `enter` opens a diff,
lower-case `r` is refresh, and a plan nobody has looked at cannot be applied.

```text
 sheep  sheep                                                                          ready
timeline claude · 5 turns · newest 1m ago · notify on
╭ rewind to #4 ────────────────────────────────────────────────────────────────────────────╮
│back to #4  2m ago · claude · manual                                          f944f1ef431a│
│lower the reconnect ceiling to 20s                                                        │
│10 paths change  —  10 written · 0 removed                                                │
│──────────────────────────────────────────────────────────────────────────────────────────│
│ will be written (10)                 ╭ src/git.rs ──────────────────────────────────────╮│
│▌+ src/git.rs                         │@@ -1,4 +1,3 @@                                   ││
│ + src/herdr/detect.rs                │-// refactor: retry helper extracted to src/retry.││
│ + src/ops.rs                         │ //! A thin, explicit wrapper over the `git` binar││
│ + src/repo.rs                        │ //!                                              ││
│ + src/shadow.rs                      │ //! Every git invocation Sheep makes goes through││
│ + src/store.rs                       │@@ -173,6 +172,8 @@ impl Git {                    ││
│ + src/tui/app.rs                     │             .with_context(|| format!("failed wait││
│ + src/tui/engine.rs                  │                                                  ││
│ + src/tui/render.rs                  │         match writer.join() {                    ││
│ + src/tui/theme.rs                   │+            // A child that exits early leaves us││
│                                      │+            // Its exit status is the real answer││
│                                      │             Ok(Err(e)) if e.kind() == std::io::Er││
│                                      │             Ok(Err(e)) => {                      ││
│                                      │                 return Err(e)                    ││
│                                      │                                                  ││
│                                      ╰──────────────────────────────────────────────────╯│
│──────────────────────────────────────────────────────────────────────────────────────────│
│ restoring rewrites 10 files and deletes 0 files under sheep/.                            │
│ the tree you have now is snapshotted first as a new turn, so this is undoable.           │
│ the agent in pane w3K:p1 will be told what was taken back.                               │
│ shift+R  restore   esc back · d diff · J/K scroll · n notify · q quit                    │
╰──────────────────────────────────────────────────────── dry run — nothing is written yet ╯
```

<sup>`sheep ui --rewind --select 4 --keys d --snapshot 92x30`, same fixture.</sup>

The same plan on the command line, which touches nothing either:

```console
$ sheep diff '#4'
restore to f944f1ef431a  ·  10 file(s) written, 0 removed
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

### It tells the agent what you took back

The message that lands in the agent's pane after a restore:

> `[sheep]` Your working tree was rewound to turn #4 (f944f1ef431a). 10 path(s) changed on disk: 10
> rewritten, 0 deleted. Anything you wrote after turn #4 is no longer on disk — re-read any file
> before you edit it, and do not re-apply the reverted changes unless you are asked to. The state
> from just before the rewind was kept as turn #6; `sheep restore #6 --yes` puts it back.

And the timeline it left behind: the checkpoint that makes the rewind reversible, and above it the
turn that records where you landed, so the log describes what is on disk rather than what you asked
for.

```console
$ sheep log
#7    9ba4396fb2cf  manual       10 files  +75     -10     restored to f944f1ef431a
#6    9766acafdf0d  checkpoint    0 files  +0      -0      before restore to f944f1ef431a
#5    90f73711129f  manual       10 files  +10     -75     extract the retry helper and use it everywhere
#4    f944f1ef431a  manual        1 files  +1      -1      lower the reconnect ceiling to 20s
#3    f0154c288942  manual        2 files  +3      -0      add a theme helper
#2    19540f92756b  manual        1 files  +2      -0      cap the note length
#1    2f0e1798bda6  manual       60 files  +0      -0      start of session
```

Outside herdr the message is a no-op by construction rather than by a branch, so the interface
behaves identically in a plain terminal.

## What Sheep never touches

Sheep overwrites files on other people's machines. One credible "it ate my uncommitted work" report
would end the project, so every promise below has a test behind it in
[`tests/adversarial.rs`](tests/adversarial.rs) — 39 tests written from the attacker's side, part of a
suite of 218.

**Your `.git`.** Snapshots live in a separate bare repository under Sheep's state directory whose
`objects/info/alternates` points at your object database, so unchanged content is *borrowed*, never
copied. Your index, HEAD, branches, stash and reflog are never written. Uninstalling is `rm -rf` on
one directory. → `never_writes_into_the_users_git_directory`

**Your hooks.** Only plumbing is used — `add`, `write-tree`, `commit-tree`, `read-tree`,
`checkout-index` — and none of it runs a repository hook, so a snapshot can never fire your
`pre-commit`. This one is held by construction rather than by a test: [`src/git.rs`](src/git.rs) is
the only place in the program that spawns git, and it strips `GIT_DIR`, `GIT_WORK_TREE`,
`GIT_INDEX_FILE` and five more from every child.

**Anything `.gitignore`d.** `git add -A` honours your ignore rules, so `.env`, `node_modules/` and
build output are outside Sheep's reach in both directions: never captured, and therefore never
overwritten. A file git *does* track is captured even when an ignore rule matches it, which is the
opposite behaviour for a case that looks identical — both are tested. →
`gitignored_files_are_never_captured_and_never_removed`,
`a_tracked_file_an_ignore_rule_matches_is_still_captured`

**Any path not in the plan.** A three-file restore rewrites three files. →
`a_restore_touches_only_the_paths_in_its_plan`

**Your line endings.** Snapshots and restores are byte-verbatim. Sheep pins `core.autocrlf=false`,
`core.eol=lf` and `core.safecrlf=false` through `GIT_CONFIG_KEY_n`, which outranks every config file,
so a machine-wide `autocrlf = input` cannot quietly normalise a CRLF file on the way in and write it
back as LF. A `.gitattributes` inside *your* repository is still honoured, because a repository that
pins its own endings should round-trip the way its own `git checkout` would. →
`line_endings_survive_a_hostile_global_gitconfig`

**A repository nested inside your worktree.** `git add -A` records one as a single gitlink — a commit
pointer, nothing more — so a restore to before it appeared produces a one-line plan,
`remove vendor/parser`, whose contents no snapshot holds. Deleting it would take that repository's
history, its uncommitted work and its ignored files with it, and the checkpoint could not bring any
of it back. Sheep refuses before touching anything:

```console
$ sheep restore '#1' --yes
restore to 9e11dca6131e  ·  0 file(s) written, 1 removed
  remove  vendor/parser
sheep: the restore failed: refusing to restore: `vendor/parser` is a directory whose contents Sheep never captured.
A git repository inside your worktree is recorded only as a pointer, so removing it here would delete files no snapshot holds — including anything ignored inside it. Move or delete it yourself if that is what you want.. Your files were put back as they were.
```

→ `a_nested_repository_is_never_deleted_by_a_restore`, `a_nested_repository_is_not_clobbered_by_a_write_either`

**A tree it cannot vouch for.** Unmerged paths, a rebase/merge/cherry-pick/revert/bisect in flight, a
directory that is not a worktree, a checkout over the file budget: refusals, not warnings. →
`refuses_a_worktree_with_unresolved_conflicts`, `refuses_a_worktree_mid_operation`,
`refuses_a_directory_that_is_not_a_worktree`, `refuses_a_worktree_over_the_file_budget`

**A snapshot with a hole in it.** Because objects are borrowed, an aggressive `git gc --prune=now` in
your repository could in principle collect one a snapshot still references. A restore checks every
object it is about to read, first, and refuses rather than leaving half a tree behind. →
`refuses_to_restore_a_snapshot_with_a_missing_object`, `a_restore_never_writes_over_bytes_no_snapshot_holds`

**The state you are in now.** It is recorded as a turn before a single byte changes, so the undo is
itself undoable. A restore removes before it writes, because a path that changes between a file and a
directory cannot be written while the old shape is in the way; if it fails in that gap, the
checkpoint is exactly what was on disk a moment ago, so Sheep replays it and then tells you which of
the two things happened. → `a_restore_is_itself_undoable`, `a_restore_that_fails_partway_puts_the_tree_back`

**The plan you read.** An agent keeps working while you are reading a plan. Sheep carries the tree the
plan was computed against into the write and refuses if it moved, handing back the plan as it stands
now rather than applying one nobody looked at. →
`a_restore_refuses_a_plan_the_tree_has_moved_out_from_under`

And nothing is written without an explicit confirmation: `--yes` on the command line, `shift+R` on a
plan that is on the screen in front of you.

## How it works

**The shadow repository.** One bare git repository per worktree, under Sheep's state directory, with
`objects/info/alternates` pointing at your object database — and at anything your repository itself
borrows from, so a `--reference` clone resolves too. A snapshot is `git add -A` into a throwaway
index, then `write-tree`, `commit-tree`, `update-ref refs/sheep/<timeline>`. Your index is never
involved. Content your repository already holds costs nothing to store; only what has changed since
your last commit is written, and only once.

**Turn detection at corroborated status edges.** A candidate opens only on `working` → at-rest, and
only for a pane that has been seen working since its last recorded turn; a pane that merely appears
at rest never produces anything. "No new output" means herdr's per-pane revision counter, which bumps
on every paint — and a still-working agent paints.

The subtle one is the directory. A turn is bound to the directory it *started* in, fixed on the edge
into `working`, because reading it when the boundary opens is already too late: a mid-turn `cd` has
been absorbed by then, and every later check compares the new directory against itself and agrees.

When the quiet window elapses the recorder corroborates twice before anything is written. It re-asks
herdr for the pane, which closes the case where the event that would have withdrawn the candidate
went missing across a reconnect. Then it asks the kernel: if the pids in the pane's foreground process
group changed, that is an agent running tools rather than an agent that has stopped, and the window
is extended. The state machine itself is pure — sightings and an explicit clock in, signals out —
which is why the whole thing is tested against synthetic event sequences with no server and no
sleeping.

**The write-back.** A restore driven from the overlay finishes by sending that pane's agent a message
over herdr's `agent.prompt`: the turn number, the file counts, the instruction to re-read, and the
checkpoint that reverses it.

## What it costs

Borrowed objects are the whole trick, and they are why recording every turn of every agent all day
does not cost anything worth noticing:

| | |
|---|---|
| five recorded turns on a 12,000-file, 132 MB checkout | **512 KB** of state |
| two everyday checkouts, 2.5 GB on disk between them — mostly ignored build output — six turns | **216 KB** of state |

Time, measured on this machine with `SHEEP_STATE_DIR` pointed at a scratch directory each run:

| | snapshot | plan (`sheep diff`) | restore |
|---|---|---|---|
| this repository — 60 tracked files | 0.11 s | 0.08 s | 0.27 s |
| that synthetic 12,000-file checkout | 1.5 s | 1.5 s | 5.9 s |

A snapshot re-stages the whole tree into a fresh index, so the *time* tracks the size of the checkout
and not the size of the change; what gets *stored* tracks the change, because everything your
repository already holds is borrowed rather than copied. A restore stages three more times — once to
compute the plan, once for the checkpoint that makes it undoable, once to record where you landed —
which is where the 5.9 s on the big fixture goes, and none of the three is optional.

`sheep gc` is the one command worth knowing about ahead of time. Trimming the turn log alone frees
nothing, because every old commit stays reachable through the parent chain; `gc` rebuilds the kept
turns as a fresh chain against the same trees and only then collects. Every kept turn still restores
to exactly the same bytes, which is what
`a_kept_turn_still_restores_to_the_same_files_after_collection` asserts. `--keep 500` and
`--max-age-days 30` by default, dry run unless `--yes`.

## Commands

The whole tool, in the order you would meet it. None of this needs herdr — `sheep` works in any git
checkout on its own, and the plugin is what makes it automatic.

```bash
sheep doctor           # can Sheep record here, and what will it leave alone
sheep snap             # record the working tree as a turn
sheep log              # the timeline
sheep diff 4           # what going back to turn 4 would do. Writes nothing
sheep restore 4 --yes  # go back. Without --yes it prints the plan and stops
sheep ui --rewind      # the same decision, with the diff in front of you
sheep gc --yes         # shorten history: rebuild the kept turns, then collect
```

A turn is named by its number, by `'#4'` if you quote it past your shell, or by its commit id.

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

`sheep doctor` is the one to run first. It answers "can Sheep record here", names anything nested,
and says out loud that your ignored files are out of reach:

```console
$ sheep doctor
worktree   /…/scratch/nested
id         nested-09dea686ace6d410
kind       main checkout
objects    /…/scratch/nested/.git/objects
state      /…/scratch/neststate
tracked    3 files
note       1 submodule(s): Sheep records the commit pointer, not the submodule's working tree.
note       gitignored files present: Sheep never captures or overwrites them.
status     ready
```

<sup>A scratch checkout with a vendored repository and an ignored `.env` in it; the two long temporary
paths are elided.</sup>

A **timeline** (`--line`) is one recording stream. `sheep watch` names one per agent per worktree by
default, so `claude` and `codex` in the same checkout do not share a history; `--line-by pane` gives
you one per pane instead, at the cost of starting fresh every time herdr restarts.

### Two things you would otherwise never find

**Put the turn number in herdr's sidebar.** After every recorded turn Sheep reports `turn = "#<n>"` to
herdr as pane metadata with a four-hour TTL. Herdr renders custom pane metadata as a `$name` token, so
one row in `~/.config/herdr/config.toml` puts each agent's current turn beside its name:

```toml
[ui.sidebar.agents]
rows = [["state_icon", "workspace", "tab"], ["agent", "$turn"]]
```

Then `herdr server reload-config`. Without that row the metadata is reported and never displayed.

**Keybindings live in your config, not in the manifest.** herdr 0.8 reads its key map from
`config.toml` alone, so the identical block in `herdr-plugin.toml` is accepted and inert. Paste
[`herdr-plugin/keybindings.toml`](herdr-plugin/keybindings.toml) into `~/.config/herdr/config.toml`
for `prefix+Z` (dock) and `prefix+z` (rewind). Until you do,
`herdr plugin action invoke dock --plugin sheep` and `… invoke rewind --plugin sheep` do the same
thing.

Other environment: `SHEEP_STATE_DIR` overrides where state lives, with precedence
`HERDR_PLUGIN_STATE_DIR` > `SHEEP_STATE_DIR` > `XDG_STATE_HOME` > `~/.local/state/sheep`. Set it when
you are experimenting so nothing lands in the real one. `SHEEP_LINE` does the same for `--line`, and
is how a dock pane learns whose timeline it is showing — a pane's argv is fixed by the manifest, so
the environment is the only thing that can tell two docks apart. If the dock and the recorder ever
named a timeline differently the dock would read one nothing writes and report an empty history,
which is indistinguishable from the truth; `tests/plugin_timeline.rs` runs both halves and compares
the strings, and the dock's empty state names the other timelines it can see for the worktree.

### Building it yourself

```bash
cargo build --release        # target/release/sheep
cargo test                   # 218 tests
cargo clippy --all-targets -- -D warnings && cargo fmt --check
```

CI runs fmt, clippy and the tests on Linux and macOS, plus shellcheck and an installer round trip
that asserts `install.sh` and the release matrix ask for exactly the same asset names. Releases are
built natively for five targets with one `SHA256SUMS`.

## What Sheep does not do

[**docs/known-limitations.md**](docs/known-limitations.md) is the short list of things that are
wrong on purpose: where the lock file loses to a backwards clock step, what happens when two agents
share one checkout, why a nested repository is a pointer and nothing else, and why a scraped prompt
is empty rather than wrong. Every entry was found by an audit written to break Sheep and then left
in deliberately, with the reasoning attached.

It is there because a tool that overwrites files and claims no limits is a tool that has not looked.

### Linux and macOS only

That is a claim rather than an omission. Sheep's recorder *is* herdr's session API, and
[`src/herdr/wire.rs`](src/herdr/wire.rs) speaks it over a unix socket with no non-unix transport
behind it: on Windows every call fails, `sheep watch` cannot record a single turn, and a dock beside
it would sit there saying nothing had happened. Sheep used to declare Windows in its manifest and
ship a whole PowerShell surface for it anyway, behind a recorder that reported "started" and exited
0. That is gone; `sheep watch` now refuses on a non-unix build instead of looping quietly, no `.exe`
is published, and there is no Windows CI leg. It comes back with the transport. The command-line half
is pure git plumbing and would very likely be fine there today — but "very likely" is not a supported
platform, and `sheep`'s state directory has no `%LOCALAPPDATA%` fallback yet.

## Prior art, and thanks

[**herdr**](https://herdr.dev) is the reason any of this is possible. The per-pane agent status, the
worktree-per-agent model and the socket API are what Sheep is built out of; without them a plugin
cannot know that an agent finished a turn, or which checkout it finished it in.

**Claude Code's `/rewind`** is the right idea and where most people meet it first.

**Cursor** keeps per-message checkpoints inside the editor. **aider** commits every change it makes
straight to git, which is the oldest and bluntest version of this idea and still a good one.
**Jujutsu** goes further than either: `jj op restore` makes the whole operation log undoable, and if
you already live in `jj` you have most of what Sheep offers, for one repository at a time.

Sheep's contribution is the awkward middle — several agents, several worktrees, several harnesses,
one machine — and telling the agent what changed underneath it.

---

MIT licensed. Removing it is `herdr plugin uninstall sheep`, then `rm -rf` on the one directory
`sheep doctor` prints as `state`. Everything Sheep ever wrote was in there, and your repository never
knew it existed.
