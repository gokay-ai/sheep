# Security

Sheep overwrites files in a user's working tree and runs `git` as them. If you have found a way to
make it destroy something, or to make it read or write outside where it says it does, please report
it privately first.

## Reporting a vulnerability

Use GitHub's private vulnerability reporting: **Security → Advisories → Report a vulnerability**, or
<https://github.com/gokay-ai/sheep/security/advisories/new>. That opens a private thread with the
maintainer; nothing is visible until an advisory is published.

Please do not open a public issue for anything that could cost somebody data.

Useful in a report, roughly in order:

1. What Sheep did that it says it never does.
2. The shape of the repository — linked worktree, nested repository, submodule, unusual ignore
   rules, unusual filenames.
3. The output of `sheep doctor` in that worktree.
4. Whether a recorder (`sheep watch`) was running at the same time.
5. A reproduction, ideally as a shell script that starts from `git init`. Every case in
   [`tests/adversarial.rs`](tests/adversarial.rs) is built that way, so one that shape can go
   straight into the suite.

The supported release is the latest tag on <https://github.com/gokay-ai/sheep/releases>.
`main` may be ahead of it.

## Threat model

Sheep runs as you, with your `git`, on your machine. It is not a sandbox and does not try to be one:
anything that can already run code as you can do everything Sheep can do, more directly. What
follows is what Sheep itself does with that access, and where the sharp edges are.

**It writes to your working tree. That is the feature.** A restore removes and overwrites the paths
in its plan. Every defence in the codebase is a narrowing of that: dry run is the default, `--yes`
or an on-screen `shift+R` is required, the plan a human read is the plan that runs, the state before
the write is checkpointed as a turn first, and a restore that fails partway is replayed from that
checkpoint automatically. The class of bug that matters is Sheep acting on **bytes it never
recorded** — that is the shape all four data-loss paths a full-tree audit reproduced in `89d28da`
had, and it is the first thing to check a finding against.

**It never writes into your `.git`.** Snapshots live in a separate bare repository under Sheep's
state directory. Your index, HEAD, branches, stash and reflog are never modified. Only git plumbing
is used, so a snapshot cannot fire a repository hook — `pre-commit`, `post-checkout` or otherwise.
[`src/git.rs`](src/git.rs) is the only place in the program that spawns `git`, and it strips
`GIT_DIR`, `GIT_WORK_TREE`, `GIT_INDEX_FILE` and five more from every child so that ambient
environment cannot redirect an invocation.

**It borrows objects rather than copying them.** The shadow repository's `objects/info/alternates`
points at your object database, so unchanged content costs nothing to snapshot. The consequence is
that an aggressive `git gc --prune=now` in *your* repository can remove an object a snapshot still
references. A restore verifies every object it is about to read before it touches anything and
refuses rather than leaving a half-restored tree.

**Snapshots are stored in the clear, with your umask.** The turn log is plain NDJSON and the shadow
repository is an ordinary git object store; neither is encrypted, and Sheep creates them with the
process umask rather than forcing `0700`. Anyone who can read your state directory can read the
contents of every tracked file at every recorded turn — including a secret that is *tracked*, and
including a screen-scraped fragment of the prompt that was on the pane. Gitignored files are the
exception, and the reason `.env` and `node_modules/` are safe: they are never captured, so they are
never stored and never overwritten. `rm -rf` on the state directory removes every trace.

**A plugin install runs code as you.** `herdr plugin install gokay-ai/sheep/herdr-plugin` runs
[`herdr-plugin/install.sh`](herdr-plugin/install.sh) as a build step. It fetches `SHA256SUMS` for
the tagged release, then the binary for your platform, and refuses to install on a mismatch or when
no `sha256sum`/`shasum` is available. Be clear about what that buys: the checksum comes from the
same GitHub release as the binary, so it defends against a truncated or corrupted download and
against a swapped individual asset — not against a compromised GitHub account or a compromised
release workflow. The download origin is not redirectable by an environment variable on a real
install. The same verified binary is copied to `~/.local/bin/sheep` so `sheep` is a shell
command; a file that is not sheep is left alone, and a failure to write the copy does not fail
the plugin install. The installer also appends `prefix+f` / `prefix+F` to herdr's
`config.toml` (herdr 0.8 ignores keybindings in the plugin manifest). It will not overwrite a
key another command already holds; it copies the file to `config.toml.sheep-bak` first and
puts that back if `herdr config check` rejects the result. `./install.sh --from-source`
builds with cargo instead if you would rather not trust a prebuilt binary.
`./install.sh --no-path` / `SHEEP_SKIP_PATH=1` skips the PATH copy;
`./install.sh --no-keys` / `SHEEP_SKIP_KEYS=1` skips the keybindings.

**The recorder talks to herdr's socket.** `sheep watch` connects to `$HERDR_SOCKET_PATH` and takes
pane status, working directories and pane text from it. Anything that can write to that socket can
tell Sheep an agent finished a turn, which at worst causes a snapshot — a read of your worktree into
your own state directory. After a restore driven from the interface, Sheep sends the agent in that
pane a message through herdr's `agent.prompt`; its text is generated by
[`src/tui/engine.rs`](src/tui/engine.rs) from a turn number, a commit id and two counts, and
contains no file contents.

**It refuses, rather than guesses, on ambiguous state.** Unmerged paths, a rebase/merge/cherry-pick/
revert/bisect in flight, a directory that is not a worktree, a checkout over the file budget, a path
that is not valid UTF-8, a removal that resolves to a directory on disk: all refusals before
anything is written.

## In scope

- Any way to make Sheep delete or overwrite a path that is not in the plan it displayed.
- Any way to destroy data no snapshot holds — nested repositories, gitignored files, a path that
  changed shape between the plan and the write.
- Writes into the user's `.git`, or any way to make Sheep run a repository hook.
- A restore that leaves the tree between two states without saying so, or that reports success when
  it did not finish.
- A path that lets one worktree's timeline restore into a different worktree.
- Anything that makes the installer accept a binary it should not have.
- Reading or writing outside the worktree and the state directory.

## Out of scope

- **The limits in [`docs/known-limitations.md`](docs/known-limitations.md).** They were found
  deliberately and left on purpose, and each says what happens when you meet it. An issue about one
  is still welcome — as an issue, not an advisory.
- Anything that requires already being able to run code as the user, or to write to their state
  directory or their repository.
- Windows. It is not supported and not claimed anywhere; `sheep watch` refuses on a non-unix build
  and no `.exe` is published.
- Denial of service against your own machine — a snapshot of a very large worktree is slow. The
  `--max-files` budget exists for the accidental version of this.
