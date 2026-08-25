# The hero GIF — a shot list

Eight seconds, one take, no cuts. The README's still frame is the timeline dock;
this is the twelve seconds of setup and eight seconds of film that turn it into
the thing people share.

Everything below is a real command against a real herdr session. Nothing is
staged in a mock: if a step does not work when you film it, that is a bug report,
not a reason to fake the frame.

## Rig

| | |
|---|---|
| outer terminal | 1280×720 logical, **120 columns × 32 rows**, font 15px, 1.25 line height |
| theme | your normal one. A screenshot in a theme nobody uses reads as a mockup |
| recorder | `asciinema rec hero.cast --cols 120 --rows 32`, then `agg --font-size 15 --theme <yours> hero.cast hero.gif` |
| target size | ≤ 3 MB. `gifsicle -O3 --lossy=60` if it is over |
| cursor | on. A blinking cursor is what tells a viewer this is a terminal and not a diagram |

## Setup — before you press record

```bash
# 1. A worktree with a real history and a real agent in it.
herdr                                     # start or attach
#    open a workspace on a repo you do not mind rewinding
#    start Claude Code (or codex, or opencode) in the left pane

# 2. Sheep, recording.
herdr plugin install gokay-ai/sheep/herdr-plugin
#    the startup hook launches `sheep watch`; confirm it is alive:
tail -f ~/.local/state/herdr/plugins/sheep/logs/watch.log

# 3. Earn a timeline. Give the agent three or four small, real tasks and let
#    each one finish. `sheep log` should show four or more `turn` rows before
#    you film anything — a two-turn timeline looks like a demo.

# 4. The turn you will rewind. Ask for something broad enough to touch nine
#    files: "extract the retry logic into a helper and use it everywhere".
#    Let it finish. Do not review it. That is the point.
```

Open the dock with `prefix+Z` and leave it open. The frame you are filming is
the agent pane on the left, the dock on the right, both already on screen.

## The eight seconds

| t | what is on screen | what you do |
|---|---|---|
| 0.0–1.2 | Left: the agent's last message, "Refactored 9 files." Right: the dock, `#12 turn claude · 9 files +214 −186` at the top, the bar wider than every row under it. | Nothing. Let it sit. The viewer needs a second to read the two panes as two panes. |
| 1.2–2.0 | The dock's turn list. | Press `prefix+z`. The rewind overlay opens over the dock on `#12`. |
| 2.0–3.0 | The overlay: `rewind to #12`, the file list, the footer. | Press `j` three times, one keystroke every ~250 ms, down to `#9` — the last turn you know was good. The header re-reads `rewind to #9` and the file count changes under it. |
| 3.0–4.2 | `will be written (9)`, then `will be removed (1)`. The footer: *restoring rewrites 9 files and deletes 1 file under \<repo\>/*. | Press `d`. The diff for the selected file opens under the list. Let one screenful of red and green land. |
| 4.2–5.0 | The footer's second line: *the tree you have now is snapshotted first as a new turn, so this is undoable.* | Nothing. This is the line that sells the tool. Hold it. |
| 5.0–5.4 | The border still says `dry run — nothing is written yet`. | Press **`shift+R`**. |
| 5.4–6.2 | The overlay closes. The dock's status band goes green: `restored to #9 — 9 written, 1 removed`, and under it `#13 checkpoint · before restore to <id>`. | Nothing. |
| 6.2–8.0 | **The left pane.** A new prompt lands in the agent by itself: `[sheep] Your working tree was rewound to turn #9 (…). 10 path(s) changed on disk: 9 rewritten, 1 deleted. Anything you wrote after turn #9 is no longer on disk — re-read any file before you edit it…` and the agent starts a `Read` on the first file. | Nothing. The last beat is the agent obeying. |

Freeze on the agent's first `Read`. Do not film its reply.

## Caption

> Rewind one agent's worktree to before it broke things — and tell it what you took back.

## What must be in frame, and what must not

**Must be visible at some point:** the two panes side by side; `dry run — nothing
is written yet` before the restore; the `checkpoint` row after it; the `[sheep]`
message arriving in the agent pane on its own.

**Must not be visible:** a real repository path with a client's name in it, a
`.env` in any file list (it cannot appear — Sheep never captures ignored files —
but a viewer who sees one will not check), an editor, a browser, or any window
that is not the terminal.

## If a take goes wrong

`sheep restore #<the checkpoint> --yes` puts the worktree back, and the agent
gets told about that too. The timeline grows two rows per take, so film with
`--line` pointed at a scratch timeline if you expect more than three attempts.
