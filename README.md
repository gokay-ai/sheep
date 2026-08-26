<h1 align="center">Sheep</h1>

<p align="center"><strong>Undo for AI coding agents.</strong></p>

<p align="center">
  <a href="https://github.com/gokay-ai/sheep/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/gokay-ai/sheep/actions/workflows/ci.yml/badge.svg"></a>
  <a href="https://github.com/gokay-ai/sheep/releases/latest"><img alt="latest release" src="https://img.shields.io/github/v/release/gokay-ai/sheep?sort=semver"></a>
  <img alt="MIT licence" src="https://img.shields.io/badge/licence-MIT-blue">
</p>

<p align="center">
  <img src="docs/readme/hero.jpg" alt="A sheep at a time-rewind machine, an agent in the wreckage behind it" width="800">
</p>

[herdr](https://herdr.dev) runs coding agents in panes, a git worktree each. Sheep records every
turn as a checkpoint, rewinds **that** worktree, and tells the agent what was taken back.

<p align="center">
  <img src="docs/readme/restore.jpg" alt="A sheep inspects a catastrophic commit while another hits rewind" width="800">
</p>

Linux and macOS.

## Install

```bash
herdr plugin install gokay-ai/sheep/herdr-plugin
```

The installer fetches a checksum-verified binary, starts the recorder, and registers the dock.
No Rust needed. Without herdr, `sheep` still works in any git checkout.

Paste [`herdr-plugin/keybindings.toml`](herdr-plugin/keybindings.toml) into
`~/.config/herdr/config.toml` for `prefix+Z` (dock) and `prefix+z` (rewind).

## Usage

```text
 sheep  sheep                                                                          ready
timeline claude · 5 turns · newest just now · notify on
╭ timeline ────────────────────────────────────────────────────────────────────────────────╮
│▌#5   manual     claude                                                b22d9624 · just now│
│▌  10 files   +10 −75   ████████████                                                      │
│▌  extract the retry helper and use it everywhere                                         │
│▌  b22d96246056 · 2026-08-26 08:24 UTC                                                    │
│ #4   manual     claude                                                  ca196040 · 1m ago│
│   1 file   +1 −1   █                                                                     │
│   lower the reconnect ceiling to 20s                                                     │
│ #3   manual     claude                                                  82aaad15 · 2m ago│
│   2 files   +3 −0   █                                                                    │
│   add a theme helper                                                                     │
│ #2   manual     claude                                                  2c937eb2 · 4m ago│
│   1 file   +2 −0   █                                                                     │
│   cap the note length                                                                    │
╰───────────────────────────────────────────────────────────────────────────────────── 1/5 ╯
j/k move · enter rewind · ? keys · q quit · n notify · r refresh
```

In the dock: `j`/`k` move, `enter` opens a plan, `shift+R` restores. Dry run is the default;
nothing is written until you confirm.

```bash
sheep doctor             # is this worktree safe to record
sheep snap               # record a turn by hand
sheep log                # the timeline
sheep diff '#4'          # what restoring turn 4 would do
sheep restore '#4' --yes # go back
sheep ui --rewind        # the same decision, with the diff in front of you
sheep gc --yes           # shorten history
```

Quote `'#4'` so the shell does not eat it. `--line` picks a timeline; `sheep watch` names one per
agent per worktree.

After a restore the agent is told what changed, and the state you were in is kept as a new turn —
so the undo is itself undoable.

## Uninstall

```bash
herdr plugin uninstall sheep
```

Then `rm -rf` the directory `sheep doctor` prints as `state`. Your `.git` is never written.
Ignored files (`.env`, `node_modules/`) are never captured and never overwritten.

MIT. How it works: [`docs/architecture.md`](docs/architecture.md). Contributing: [`AGENTS.md`](AGENTS.md).
