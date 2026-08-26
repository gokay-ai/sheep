# Known limitations

Everything here was found deliberately, by audits written to break Sheep, and left in on purpose
rather than missed. Each entry says what the limit is, when you would meet it, and what happens.

If you hit one of these, an issue is still welcome — the point of writing them down is so the
conversation can start from what is already understood.

## The lock, and a system clock that steps backwards

Sheep serialises writers to one worktree's history with a lock file under its state directory. A
live holder re-stamps that file every few seconds, so a lock nobody has touched for thirty
seconds is treated as debris from a killed process and broken.

A **backwards** system clock step larger than thirty seconds — an NTP correction, a VM resuming
from a snapshot — makes a live holder's freshly-stamped file read as future-dated, which is also
treated as debris. Two writers can then run against one state directory for as long as the clock
takes to catch up.

The trade is deliberate and it went this way round: judging a future-dated file *live* instead
would let a killed process wedge a worktree permanently, and a wedge announces itself loudly
while a race does not. The proper fix is to judge staleness by watching whether the stamp moves
rather than by comparing it to the clock, and it is the first thing to change here.

## `gc` beside a writer that is not holding the lock

`sheep gc` reads the timeline's ref and then its log, and refuses to collect if the ref moved
underneath it. A turn recorded between those two reads is still, in principle, droppable — but
only by a writer that is not taking the lock at all, which today means a different, older Sheep
binary running against the same state directory.

Sheep's own callers all take the lock, so this needs two versions installed at once to reach.

## Nested repositories

A git repository inside your worktree — a vendored checkout, or a directory an agent ran
`git init` in — is recorded the way git records it: as a commit pointer and nothing else. Sheep
captures nothing inside it, restores nothing inside it, and **refuses** any restore that would
remove or overwrite it, because the contents are in no snapshot and the checkpoint could not
bring them back.

`sheep doctor` names them before you start, at any depth: the scan reads `ls-files -o` without
`--directory`, so git descends into untracked parents and emits a trailing slash only at a
repository boundary. A nested repository with no commit checked out is a refusal, not a note —
`git add -A` cannot index it, so every snapshot would otherwise die with a raw git error after
doctor had said `ready`. Commit inside it, or remove it.

The refusal to remove or overwrite one is decided at the moment of the write, against what is
on disk, not against what doctor found.

## Ignored files

Anything `.gitignore` matches and git does not track is outside Sheep's reach in both directions:
never captured, never overwritten. That is what keeps `.env` and `node_modules/` safe, and it
means Sheep cannot restore them either.

A file git *does* track is always captured, even when an ignore rule matches it. The two cases
look similar and behave oppositely, which is why they are both stated here.

## Line endings

Snapshots and restores are byte-verbatim: Sheep pins `core.autocrlf=false` for its own git
invocations, so the machine's global gitconfig cannot normalise your files on the way in or out.
A `.gitattributes` **inside your repository** is still honoured, so a repository that pins its own
line endings round-trips the way its own `git checkout` would.

## Two agents in one checkout

Sheep snapshots whole worktrees. Two agents working in one checkout share a timeline, and
rewinding one takes back the other's in-flight edits — the plan shows every path before anything
happens, and the pre-restore checkpoint gets it back, but the model cannot express "only that
agent's changes". herdr's own model is a worktree per agent, which is where Sheep is at its best.

## Platforms

macOS and Linux. Windows is not supported and is not claimed anywhere: every call into herdr goes
through a unix socket, so the recorder and the interface cannot work there. The command-line half
is pure git plumbing and would very likely be fine once the state directory learns about
`%LOCALAPPDATA%` — "very likely" is not a supported platform, so it is not claimed.

## Prompt capture

The prompt shown beside a turn is read off the pane, because no API tells a plugin what a user
typed. It is labelled as screen-scraped everywhere it appears, and it is empty rather than wrong
when the prompt has scrolled away.
