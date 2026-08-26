# How Sheep works

The README says what Sheep does. This is how, and — more usefully — why each part is shaped the way
it is. Almost every decision below was made because the obvious version of it lost somebody's work
in a way a test could reproduce; where that is true, the commit that fixed it is named.

Three pieces, in dependency order:

```
  the recorder                  the interface
  src/herdr/                    src/tui/
  watches herdr, decides        shows a plan, applies it,
  a turn ended                  tells the agent what changed
        \                            /
         \                          /
          v                        v
              src/ops.rs
              snap · plan · restore · collect
              the only implementation of any of them
                        |
        +---------------+---------------+
        |               |               |
   src/shadow.rs    src/store.rs    src/lock.rs
   the object       the turn log    one writer at a
   store            (NDJSON)        time per worktree
        \               |               /
              src/git.rs — the only place that spawns git
```

The CLI (`src/main.rs`) is argument parsing over the same `ops` functions. Nothing anywhere calls a
parallel implementation of a snapshot or a restore: there is one, so a fix lands once.

## The state directory

Everything Sheep keeps lives under one directory, and `rm -rf` on it removes every trace.

```
<state>/
  turns/<worktree-id>/<timeline>.ndjson    the turn log, append-only, one file per timeline
  shadow/<worktree-id>.git                 the bare repository holding the snapshots
  tmp/                                     scratch index files, one per operation
  locks/                                   one advisory lock file per worktree
  logs/watch.log                           the recorder's log, under herdr
```

`<worktree-id>` is `<directory name>-<8 bytes of sha256 of the absolute path>`
([`src/repo.rs`](../src/repo.rs), `worktree_id`). The hash is not decoration: a herdr session is
made of linked worktrees, several of which may be called `fix`, and without it `restore #1` in one
writes the other's tree over it (`51d4d5f`).

Location is decided by `state_dir_from`, first variable that is set: `HERDR_PLUGIN_STATE_DIR`,
`SHEEP_STATE_DIR`, `XDG_STATE_HOME`, then the platform's home. An empty variable counts as unset,
because a shell that passes an unset variable on as `""` would otherwise put the state directory
somewhere relative to whatever directory the process started in.

## The shadow repository

### Why a separate repository at all

The user's `.git` is off limits. Recording into it would mean touching their index, their refs and
their reflog, and one of Sheep's promises is that uninstalling costs nothing and leaves nothing.
So each worktree gets a bare repository of its own under `<state>/shadow/`.

That would normally mean copying every file into a second object store. It does not, because of one
line:

```
<state>/shadow/<id>.git/objects/info/alternates  →  <the user's>/.git/objects
```

An alternate makes the user's object database readable from the shadow. Everything the user has
already committed is therefore already present, for free; only what has changed since their last
commit is written, and only once. If the user's own repository borrows from somewhere — a
`--reference` clone — the shadow follows that chain too, or the objects it thinks it has would not
resolve ([`src/shadow.rs`](../src/shadow.rs), `write_alternates`).

The cost of borrowing is stated in [known-limitations](known-limitations.md) and defended in
[`apply`](#applying-it): the user can run `git gc --prune=now` and take a borrowed object away.

### Plumbing only

`add`, `write-tree`, `commit-tree`, `read-tree`, `checkout-index`, `diff-tree`, `cat-file`,
`update-ref`. None of them runs a repository hook, so a snapshot can never fire somebody's
`pre-commit`. This holds because [`src/git.rs`](../src/git.rs) is the only place in the program that
spawns `git` and it never reaches for porcelain — there is no test for it, and there cannot be one;
it is a property of the code, not of its behaviour.

Every invocation is scoped with an explicit `--git-dir`/`--work-tree` pair and has eight `GIT_*`
variables removed from its environment, so ambient state cannot redirect it. The shadow's own
handle additionally pins `core.autocrlf=false`, `core.eol=lf` and `core.safecrlf=false` through
`GIT_CONFIG_KEY_n`, which outranks every config file. Without that, a machine with the routine
`autocrlf = input` records a CRLF file as LF, writes it back as LF, normalises the checkpoint on
the way in too — and then `sheep snap` reports "nothing changed", so Sheep cannot even see the
damage it did (`89d28da`). A `.gitattributes` *inside* the repository is deliberately still
honoured, so a repository that pins its own line endings round-trips the way its own `git checkout`
would.

### Taking a snapshot

```
git add -A -- .                     into a throwaway index under <state>/tmp/
git add -A -f --pathspec-from-file  every path the user's repo tracks, forced
git write-tree                      → a tree object
git commit-tree                     → a commit, parented on the timeline's head
git update-ref refs/sheep/<line>    compare-and-swap
append one line to <timeline>.ndjson
```

The index is a scratch file, never the user's. `-A` honours `.gitignore`, which is the whole of
invariant 8: ignored files are never captured, and therefore never restored over.

The second `add` is the subtle one. A scratch index starts *empty*, so every path in it looks
untracked — and git applies ignore rules only to untracked paths. Real git never hits this because
its index already knows what is tracked. The consequence was that a file the repository tracks but
some exclude rule matches was in no snapshot at all; and when a rule started matching a file an
earlier turn *had* captured (an agent tidying a `.gitignore` mid-session), a restore wrote stale
bytes over a live secret and the offered undo then deleted it. So tracked paths are staged again,
explicitly and forcibly — and only those still present as files, because force-adding a path that
has become a directory would drag every ignored thing beneath it in (`89d28da`).

A tree byte-identical to the previous turn is not recorded unless `allow_empty` is set: an agent
that answered a question without editing anything should not get a row.

### The turn log

One append-only NDJSON file per timeline, one object per line: sequence number, kind
(`turn`/`checkpoint`/`manual`), commit, tree, parent, timestamp, file and line counts, and the
optional pane, agent, prompt and note.

NDJSON rather than SQLite for a reason that runs through the whole project: no dependency may need
a C toolchain, because the binary has to cross-compile to five targets without one on the user's
machine. The other half of the trade is that a corrupted tail costs one line instead of a database.

`#7` in `sheep restore #7` is the sequence number, and it is minted and written under the lock so
that a number and the turn claiming it are one step. Reading the last turn does not parse the whole
file — it reads backwards in blocks — because the recorder asks for it on every snapshot, which
made appending quadratic over the life of a daemon (`d45af16`).

A timeline name has to be both a filename and a git ref. `store::slug` is the single mapping both
callers share; it passes legal names through unchanged and otherwise rewrites and appends three
bytes of digest, because the rewrite is lossy and `w3:p1` and `w3/p1` would otherwise collapse onto
one ref and interleave two agents' histories (`41fdaec`).

### Planning a restore

```
current = write_tree()                             the worktree as it is right now
diff-tree -r -z --no-renames --name-status current target
    D <path>  → remove
    *  <path> → write
```

That is the whole plan, and it is what `sheep diff` prints and what the rewind overlay draws. It
touches nothing on disk. `current` travels with the plan, which is what makes the next part
possible.

### Applying it

`ops::restore_expecting` takes the tree the plan was computed against. It re-plans under the lock,
immediately before the write, and if the tree has moved it abandons the restore and hands back the
plan as it stands now. A user who read a three-file plan cannot be given a nine-file one because the
agent kept working while they were reading (`92a3c1d`). The comparison and the application are the
same object with nothing in between.

Then, in order:

1. **Verify.** Every object the plan will read is checked with `cat-file --batch-check`. A missing
   one — the borrowed-object case — is a refusal, not a partial restore.
2. **Refuse anything not ours to touch.** *Nothing may be written over, or removed from, bytes no
   snapshot holds.* For each path the plan writes: if something is on disk there and the tree just
   recorded does not hold that path as a blob, those bytes are in no snapshot. That single statement
   covers a nested repository, a directory, and a file an exclude rule hid — and it cannot fire on
   an ordinary overwrite or an ordinary new file. It is stated as the invariant rather than as a
   list of shapes because the list was what was wrong: the removal side already refused
   directories, but `checkout-index -f` clears its own ground (git's `remove_subtree` fires whenever
   a write target is a directory), so a one-line plan `write vendor` recursively deleted a nested
   repository (`89d28da`).
3. **Refuse a path that cannot be named.** A lossily-decoded path made a removal silently no-op
   while Sheep reported a deletion that did not happen.
4. **Refuse a removal that is a directory on disk.** `diff-tree -r` reports leaves, so a removal is
   always a single file; a removal that resolves to a directory is either a nested repository or a
   path that turned into one between the plan and the write. `git add -A` records a repository
   inside the worktree as a single gitlink — a commit pointer, nothing else — so restoring past the
   point it appeared produces a one-line plan whose contents no snapshot holds. Deleting it would
   take that repository's history, its uncommitted work and its ignored files, and the checkpoint
   could not bring any of it back. This was the worst bug the project has had (`f743f1e`).
5. **Remove, then prune emptied directories, then write.** Removals go first, and have to: a path
   changing between a file and a directory cannot be written while the old shape is still there.
   Writes are `read-tree` into a scratch index followed by `checkout-index -f -u --stdin -z` with
   exactly the plan's paths on stdin — nothing else in the checkout is touched.

Because step 5 removes before it writes, a failure in the middle leaves a tree that is neither
state. The state before the attempt was checkpointed as a turn before step 1, so a plan from here to
there is precisely the repair: it runs automatically, and `ops::RestoreFailed` reports which of the
two things happened — "your files were put back" or "your tree is between two states and here is the
turn that returns it". What it can never say is that nothing was written (`fe7141f`, `b282ba9`).

Recording where the restore landed happens *after* the files are on disk. A failure there — a full
state directory, a merge started in the second the restore took — is bookkeeping, not the restore,
and is carried back as `Restored::bookkeeping_error`. Returning an error would tell someone their
files were as they were when they were not, and send them to undo something that had worked
(`0b80a8a`).

### Forgetting

A recorder is meant to run for days, so history has to be shortenable. Trimming the turn log alone
frees nothing: every old commit stays reachable through the parent chain, so the oldest kept turn
has to become a root before anything earlier can be collected.

`sheep gc` therefore *rebuilds*. `shadow::rechain` walks the kept turns oldest-first and writes a
fresh commit for each against **the same tree object**, so every kept turn still restores to exactly
the same bytes; the log is rewritten with the new commit ids, the ref is moved with a
compare-and-swap, and only then does `collect` run `reflog expire --expire=now --all` and
`gc --prune=now` on the shadow — never on the user's repository, whose borrowed objects it cannot
reach. Turn numbering continues rather than restarting, and a timeline is never left empty
(`d45af16`).

`ops::collect` reads the timeline's **ref before its log**, and the order is load-bearing. The other
way round, a turn appended between the two reads is absent from the log but already in the ref, so
the compare-and-swap succeeds and the rewrite drops it. Read the ref first and that same turn is in
the log the collection keeps (`412a98e`). [`tests/collect_read_order.rs`](../tests/collect_read_order.rs)
manufactures a seam between two adjacent statements by stalling the first read with a stubbed `git`
on `PATH`, which is why it is a test binary of its own.

### One writer at a time

`sheep watch` stays alive for a whole session, so every `sheep gc` and every `sheep restore` a user
runs happens beside a recorder that may append at any moment. `ops::collect` is a read-modify-write
of an append-only log that takes seconds; anything appended inside that window was dropped from the
log and its objects collected — including the checkpoint a restore had just promised the user by
number (`b760793`).

[`src/lock.rs`](../src/lock.rs) is one advisory lock **per worktree**, not per timeline. A timeline
owns a file and a ref, so per-timeline looks like the natural grain, and it is not enough: all of a
worktree's timelines share one shadow repository, and `gc --prune=now` on it deletes every object no
ref reaches — including the tree another timeline wrote a millisecond ago and has not yet pointed a
commit at. The unit that must be exclusive is the unit `gc` operates on.

The shape is `create_new` on a file under `<state>/locks/` — one atomic syscall, no C dependency —
with a heartbeat every 5 s so a live holder is never mistaken for debris, and a rename to break what
a killed process left behind after 30 s. Staleness counts a stamp that is too far in the *future* as
debris too, or a backwards clock step leaves a file no heartbeat can ever catch up with and the
worktree is wedged permanently (`412a98e`). That trade, and the race it admits instead, is written
down in [known-limitations](known-limitations.md).

Waits differ by caller and the reason is in [`src/ops.rs`](../src/ops.rs): a snapshot waits 5 s,
because the recorder must never stall a session and a missed turn is recoverable — the next one
records the same tree. A restore or a collection waits 60 s, because a person asked for it and is
watching, and neither has anything useful to do without the lock.

`update-ref` is a compare-and-swap in both `commit` and `rechain`, and `store::append` refuses a
sequence number that is not past the end of the log. Neither is load-bearing while the lock is
there; both turn the race into a clean error if it ever is not — an older Sheep binary running
against the same state directory, say.

## Turn detection

### Why the obvious boundary is wrong

Herdr publishes a status per pane, and infers it from what the pane paints. The naive turn boundary
is `working` → `idle`/`done`, and it is wrong often enough to matter: herdr calls agents `done`
mid-turn. A live herdr 0.8.0 session was measured flipping a pane to `done` and back to `working`
9.2 seconds later with the agent still working.

A turn Sheep invents is worse than one it misses — an invented one pollutes the list a user has to
pick from — so the detector is built for precision and accepts the misses.

### The rule

[`src/herdr/detect.rs`](../src/herdr/detect.rs) is a pure state machine over sightings and an
explicit `Instant`. It has no socket and no clock of its own, which is why
[`tests/detect_boundaries.rs`](../tests/detect_boundaries.rs) drives every case in microseconds with
no server and no sleeping.

1. A candidate opens only on `working` → at-rest, and only for a pane that has actually been seen
   `working` since its last recorded turn. A pane that merely appears at rest produces nothing.
2. The candidate must survive a **quiet window** — `--settle`, 10 s by default — with no status
   change and no new output. "No new output" is herdr's per-pane `revision` counter, which it bumps
   every time the pane paints; a still-working agent paints.
3. Any move back to `working` withdraws it. That is the whole defence against the false `done`.
4. `blocked` and `unknown` withdraw it too: an agent waiting on the user has not finished a turn,
   and `unknown` means herdr has lost the thread.
5. A turn is bound to the directory it *started* in, fixed on the edge **into** `working` and held
   until the turn resolves. A pane that moves at any point while a turn is in flight abandons it,
   rather than filing it against a repository the agent never touched. Reading the directory when
   the boundary opens is too late — by then the move has been absorbed and every later check
   compares the new directory against itself and agrees (`f149564`).
6. When the window elapses the recorder gets `Signal::Ripe` and corroborates before anything is
   written. Corroboration may ask to wait, but only until `--patience` runs out.

### Corroboration

Everything that needs the socket or the kernel is the recorder's job, and comes back into the state
machine as a `Verdict` ([`src/herdr/recorder.rs`](../src/herdr/recorder.rs), `settle`):

- **Ask herdr again**, rather than trusting the last event seen. Still working? Drop. Pane gone?
  Drop. Different working directory? Drop — this closes the case where the move event went missing
  across a reconnect.
- **Ask the kernel.** If no agent process is in the pane's foreground process group, drop. If the
  foreground program changed under us, drop. If the *pids* in that group changed, that is an agent
  running tools, not an agent that has stopped: wait, and extend the window. This is why a pane
  that is busy spawning things is patiently waited out rather than losing its turn.
- **The backstop.** The recorder keeps its own record of which worktree each turn began in and
  refuses to record anywhere else. It deliberately shares no reasoning with rule 5 above — a
  backstop that reasons like the thing it backs up would have absorbed the same move.

One more failure mode shaped this code: `Live::optional` used to map every server-reported error to
"the thing is not there", so a single `invalid_request` — a herdr without the method, a params
mistake — dropped every turn from then on, in silence, while the log still said everything was
fine. Only `pane_not_found` and `agent_not_found` mean absence now; everything else is a fault. So
is a success envelope missing a key it promises, because a renamed field on a protocol bump would
otherwise drop every turn for ever (`66451a7`, `f149564`).

The baseline for a pane is taken at the **first sighting** of it, not when its first turn starts.
Herdr infers `working` from paint, so the edge arrives after the agent has already begun; a baseline
taken there captures the agent's first write, the boundary compares equal, and a real turn goes
unrecorded while the file on disk has plainly changed (`2ee49eb`).

### One subscription, and timeline naming

`pane.agent_status_changed` is per-pane, which would mean subscribing again for every pane that
appears. The parameterless `pane.updated` carries the whole `PaneInfo` — status, cwd, agent, and the
output revision the quiet window watches — so one subscription covers panes that do not exist yet
(`c20f8f8`). [`src/herdr/supervise.rs`](../src/herdr/supervise.rs) is the reconnect policy, as a
value that takes the clock as an argument so hours-long states can be tested in microseconds.

`--line-by` names the timeline: `agent` by default (one per agent per worktree), or `pane`.
`LineBy::timeline` is the single statement of the rule. The plugin has to arrive at the *same*
string from the other side — `sheep_target_line` in
[`herdr-plugin/scripts/common.sh`](../herdr-plugin/scripts/common.sh) reads `focused_pane_agent` out
of herdr's invocation context and passes it to a dock pane as `SHEEP_LINE`. When the two disagree,
the dock reads a timeline nothing writes and says "nothing recorded yet", which is indistinguishable
from the truth — the most damaging sentence this plugin can print.
[`tests/plugin_timeline.rs`](../tests/plugin_timeline.rs) therefore runs both halves for real, in a
shell and through clap, and compares the strings and the turn-log paths they open (`25f825a`).

The prompt beside a turn is screen-scraped, because no API tells a plugin what a user typed. The
rule is deliberately narrow — the last visible line starting with a prompt marker and carrying real
text, refusing anything that looks like an input box or a placeholder — and `Turn.prompt` stays
empty rather than holding noise. It is labelled as scraped everywhere it is shown.

## The interface

[`src/tui/`](../src/tui) is split so that the dangerous part is testable without a terminal:

- `app.rs` decides and never blocks. `Confirm` is a no-op unless a plan for the selected turn is on
  screen, so the dry run is enforced by the state machine rather than by the drawing code.
- `engine.rs` is a worker thread: plan, fetch hunks, restore, write back.
- `render.rs` draws and reads nothing.
- `runtime.rs` is the event loop with its three edges — screen, keyboard, worker — behind traits.

That last split exists because two of the interface's promises live only in the loop and nowhere
else. Keys typed during a restore must not act on what the restore leaves behind: a `Reply::Stale`
puts a *different* plan on screen, so a queued `shift+R` could confirm a plan nobody read. And
nothing may exit on top of a write in progress — every `?` in the loop was once an exit that ignored
an in-flight restore, possibly between `apply`'s deletions and its writes, reachable without anyone
pressing a key (`c79ff1d`, `b55a8d2`).

`sheep ui --snapshot 92x18` renders one frame into an off-screen buffer and writes it to stdout as
text. That is what makes a screenshot reviewable in a pull request and assertable in CI with no
pseudo-terminal involved.

## The write-back

The part nothing else in this ecosystem does. After a restore driven from the overlay, Sheep sends
the agent in that pane a message over herdr's `agent.prompt`:

> `[sheep]` Your working tree was rewound to turn #4 (75896f9e7a2f). 10 path(s) changed on disk: 10
> rewritten, 0 deleted. Anything you wrote after turn #4 is no longer on disk — re-read any file
> before you edit it, and do not re-apply the reverted changes unless you are asked to. The state
> from just before the rewind was kept as turn #6; `sheep restore #6 --yes` puts it back.

It is generated by `engine::rewind_message` from a turn number, a commit id and two counts —
nothing from the files themselves. Outside a herdr session `wire::try_request` answers `Ok(None)`
and this is a no-op **by construction rather than by a branch**, so the interface behaves identically
in a plain terminal. A timeline can also carry a pane id from a session that has ended, which is why
the overlay's footer says whether anyone will actually be told.

Sheep does not restore a conversation. It restores files, and then tells the conversation.

## Why the seams are where they are

Every hard part of this program is reachable from a test that needs no herdr, no terminal and no
agent, and that is a design constraint rather than an accident:

| seam | what it buys |
|---|---|
| `detect.rs` takes sightings and an explicit clock | every boundary rule tested against synthetic sequences, in microseconds |
| `session.rs` is a trait | the recorder is driven end to end by a scripted herdr that can express a pane it answers nothing for |
| `supervise.rs` is a value taking the clock | reconnect states that take hours in reality are unit tests |
| `runtime.rs`'s three edges are traits | a scripted keyboard, a screen that fails on demand, a worker that answers on cue |
| `render.rs` reads nothing | frames asserted through `ratatui`'s `TestBackend`, no pty |
| `ops.rs` is the only implementation | the adversarial suite covers the CLI, the recorder and the interface at once |

The one thing deliberately left untestable is invariant 2 — that Sheep runs no repository hooks.
Nothing can test it, because there is no behaviour to observe; it holds because `src/git.rs` is the
only place that spawns `git` and it spawns plumbing.
