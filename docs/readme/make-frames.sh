#!/usr/bin/env bash
# Regenerate every block in README.md that comes off a recorded timeline: the
# two interface frames, the plan, the restore and the log it leaves behind.
#
# They have to be real output, so this builds a throwaway five-turn timeline in
# a temporary clone of this repository, photographs it with `sheep ui
# --snapshot`, and then actually rewinds it. Nothing outside $TMPDIR is written:
# the clone, the state directory and the timeline all live under one directory
# that is removed on the way out.
#
#   ./docs/readme/make-frames.sh            build, then print both frames
#   ./docs/readme/make-frames.sh --keep     leave the fixture in place
#
# The turn ages in the frames come from the sleeps below. They are what make a
# recorded session look like a session rather than five snapshots taken in one
# second; shorten them with SHEEP_FRAME_SPEED=0 if you only care about layout.

set -euo pipefail

repo_root=$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)
readonly repo_root
readonly sheep="$repo_root/target/release/sheep"
speed=${SHEEP_FRAME_SPEED:-1}
keep=0
[ "${1:-}" = "--keep" ] && keep=1

[ -x "$sheep" ] || {
  echo "build it first: cargo build --release" >&2
  exit 1
}

work=$(mktemp -d)
[ "$keep" = 1 ] || trap 'rm -rf "$work"' EXIT

export SHEEP_STATE_DIR="$work/state"
fixture="$work/sheep"
git clone --quiet "$repo_root" "$fixture"
cd "$fixture"

snap() { "$sheep" snap --line claude --agent claude --pane w3K:p1 --note "$1" >/dev/null; }
pause() { [ "$speed" = 0 ] || sleep "$(($1 * speed))"; }

snap "start of session"
pause 150

# Four turns of an agent doing small, plausible things, and then one turn of it
# doing something broad and wrong — which is the turn worth rewinding.
printf '\n// note: kinds are stable on the wire; never renumber\n' >> src/store.rs
snap "cap the note length"
pause 130

printf '\n// theme: one place that decides a colour\n' >> src/tui/theme.rs
printf '\n' >> src/tui/text.rs
snap "add a theme helper"
pause 95

sed -i.bak 's/MAX_BACKOFF: Duration = Duration::from_secs(30)/MAX_BACKOFF: Duration = Duration::from_secs(20)/' \
  src/herdr/supervise.rs && rm -f src/herdr/supervise.rs.bak
snap "lower the reconnect ceiling to 20s"
pause 70

# The bad turn: a "tidy up the comments" pass that strips nine ordinary comments
# out of each of ten files and leaves a claim about a helper behind.
python3 - <<'PY'
files = ['src/git.rs', 'src/ops.rs', 'src/repo.rs', 'src/shadow.rs', 'src/store.rs',
         'src/herdr/detect.rs', 'src/tui/app.rs', 'src/tui/engine.rs',
         'src/tui/render.rs', 'src/tui/theme.rs']
for path in files:
    kept, dropped = [], 0
    for line in open(path).read().split('\n'):
        stripped = line.strip()
        ordinary = stripped.startswith('//') and not stripped.startswith(('///', '//!'))
        if dropped < 9 and ordinary:
            dropped += 1
            continue
        kept.append(line)
    kept.insert(0, '// refactor: retry helper extracted to src/retry.rs')
    open(path, 'w').write('\n'.join(kept))
PY
snap "extract the retry helper and use it everywhere"

echo "=== sheep ui --line claude --snapshot 92x18 ==="
"$sheep" ui --line claude --snapshot 92x18
echo
echo "=== sheep ui --line claude --rewind --select 4 --keys d --snapshot 92x30 ==="
"$sheep" ui --line claude --rewind --select 4 --keys d --snapshot 92x30
echo
echo "=== sheep diff --line claude '#4' ==="
"$sheep" diff --line claude '#4'

# The frames above are taken before anything is written; everything below is
# after the rewind, which is why it has to come last.
echo
echo "=== sheep restore --line claude '#4' --yes ==="
"$sheep" restore --line claude '#4' --yes
echo
echo "=== sheep log --line claude ==="
"$sheep" log --line claude

[ "$keep" = 1 ] && echo && echo "fixture kept at $work"
exit 0
