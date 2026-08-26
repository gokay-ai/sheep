# Changelog

User-visible changes only, newest first. The reasoning behind each one is in the commit that made
it; `git log` is the long version, and the GitHub release notes are generated from it. This file is
for the question a release page cannot answer: *does upgrading change what Sheep will do to my
files?*

## Unreleased

Nothing has been tagged yet. This is what `v0.1.0` will contain.

### The command line

- `sheep snap`, `log`, `diff`, `restore`, `doctor`, `gc` — record a worktree as a turn, list turns,
  see exactly what restoring one would do, go back, check whether a worktree is safe, and shorten
  history.
- Dry run is the default. `sheep restore` prints its plan and writes nothing without `--yes`.
- `sheep gc` rebuilds the kept turns as a fresh chain against the same trees before collecting, so
  every kept turn still restores to exactly the same bytes. `--keep 500` and `--max-age-days 30` by
  default.
- `-C/--repo`, `--line` (also `SHEEP_LINE`), `--max-files` (60,000 by default).
- `SHEEP_STATE_DIR` chooses where state lives; precedence is `HERDR_PLUGIN_STATE_DIR` >
  `SHEEP_STATE_DIR` > `XDG_STATE_HOME` > the platform's home.

### The recorder

- `sheep watch` holds one subscription to herdr and records a turn whenever an agent finishes one,
  across every pane in the session. One timeline per agent per worktree by default; `--line-by pane`
  for one per pane.
- A boundary must survive a quiet window (`--settle`, 10 s) and then be corroborated against both
  herdr and the pane's foreground process group before anything is written.
- Turns are reported to herdr as pane metadata, so `$turn` can be rendered in the sidebar.

### The interface

- `sheep ui` is the timeline dock; `sheep ui --rewind` is the plan-and-restore overlay. A restore is
  reachable only from a plan that is on the screen, and only with `shift+R`.
- After a restore, the agent in that pane is told over herdr's `agent.prompt` what was taken back
  and which turn puts it back. Outside herdr this is a no-op.
- `sheep ui --snapshot <cols>x<rows>` writes one frame to stdout as plain text.

### The herdr plugin

- `herdr plugin install gokay-ai/sheep/herdr-plugin` installs a checksum-verified prebuilt binary,
  registers the dock, the rewind overlay and the recorder, and never needs Rust.
  `--from-source` builds with cargo instead.
- `herdr-plugin/keybindings.toml` is `prefix+Z` and `prefix+z`, ready to paste into
  `~/.config/herdr/config.toml`. herdr 0.8 reads keybindings from your own config, not from a plugin
  manifest.

### Platforms

- Linux and macOS. Five release targets: `aarch64-apple-darwin`, `x86_64-apple-darwin`,
  `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, `x86_64-unknown-linux-musl`, with one
  `SHA256SUMS`.
- Windows is not supported and not claimed: every call into herdr goes over a unix socket, so
  `sheep watch` refuses at startup on a non-unix build and no `.exe` is published.

### Safety

The eight invariants in [`AGENTS.md`](AGENTS.md), each with a test written from the attacker's side
in [`tests/adversarial.rs`](tests/adversarial.rs). Sheep never writes into your `.git`, never runs a
repository hook, never captures or overwrites a gitignored file, never touches a path outside the
plan it showed you, refuses to delete a repository nested in your worktree, verifies every object
before it reads it, checkpoints your tree before it changes a byte, and puts the tree back if a
restore fails partway.

Known limits, all found deliberately and left on purpose, are in
[`docs/known-limitations.md`](docs/known-limitations.md).
