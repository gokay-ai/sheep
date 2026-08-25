#!/usr/bin/env bash
# Prove install.sh picks the right asset, and that the right asset exists.
#
#   test-install.sh                     platform detection + the release.yml round trip
#   test-install.sh --release-dir DIR   also check a staged release directory
#
# The round trip is the point. install.sh maps (uname -s, uname -m, libc) to an
# asset name; .github/workflows/release.yml maps a build matrix to asset names.
# Nothing links the two but a string, and a mismatch is invisible until someone
# runs `herdr plugin install` and gets a 404. So this asserts the two sets are
# equal, in CI on every push and again in the release job against the files that
# are about to be uploaded.
#
# Needs no network: platform detection runs against faked uname output behind
# SHEEP_TEST_MODE, and the download path is exercised over file:// URLs.

set -eu

here=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
plugin_root=$(CDPATH='' cd -- "$here/.." && pwd)
repo_root=$(CDPATH='' cd -- "$plugin_root/.." && pwd)
installer="$plugin_root/install.sh"
workflow="$repo_root/.github/workflows/release.yml"

release_dir=""
while [ $# -gt 0 ]; do
  case "$1" in
    --release-dir)
      shift
      release_dir=${1:?--release-dir needs a path}
      ;;
    *)
      echo "usage: test-install.sh [--release-dir DIR]" >&2
      exit 2
      ;;
  esac
  shift
done

failures=0
check() {
  label=$1
  expected=$2
  actual=$3
  if [ "$expected" = "$actual" ]; then
    printf 'ok    %s -> %s\n' "$label" "$actual"
  else
    printf 'FAIL  %s -> %s (expected %s)\n' "$label" "$actual" "$expected" >&2
    failures=$((failures + 1))
  fi
}

# --- 1. platform detection ---------------------------------------------------

resolve_asset() {
  SHEEP_TEST_MODE=1 \
    SHEEP_TEST_UNAME_S="$1" \
    SHEEP_TEST_UNAME_M="$2" \
    SHEEP_TEST_LIBC="$3" \
    bash "$installer" --dry-run </dev/null | awk '$1 == "asset" { print $2 }'
}

echo "== platform detection =="
# uname -s | uname -m | libc | expected asset
while IFS='|' read -r os arch libc expected; do
  case "$os" in '' | '#'*) continue ;; esac
  actual=$(resolve_asset "$os" "$arch" "$libc")
  check "$os/$arch${libc:+/$libc}" "$expected" "$actual"
done <<'CASES'
Darwin|arm64||sheep-aarch64-apple-darwin
Darwin|aarch64||sheep-aarch64-apple-darwin
Darwin|x86_64||sheep-x86_64-apple-darwin
Linux|x86_64|gnu|sheep-x86_64-unknown-linux-gnu
Linux|amd64|gnu|sheep-x86_64-unknown-linux-gnu
Linux|x86_64|musl|sheep-x86_64-unknown-linux-musl
Linux|amd64|musl|sheep-x86_64-unknown-linux-musl
Linux|aarch64|gnu|sheep-aarch64-unknown-linux-gnu
Linux|arm64|gnu|sheep-aarch64-unknown-linux-gnu
CASES

# Platforms with no prebuilt asset must say so rather than guess. Two are worth
# naming: aarch64 musl, where an aarch64 machine that is not glibc must NOT be
# handed the gnu binary because it cannot start it; and Git Bash / MSYS2, which
# used to resolve a windows-msvc asset. Sheep does not build one — the recorder
# is herdr's unix-socket API and there is no transport for it on Windows — so
# these must fall through to "none" rather than install a binary that starts and
# then records nothing.
echo "== platforms with no asset =="
while IFS='|' read -r os arch libc; do
  case "$os" in '' | '#'*) continue ;; esac
  actual=$(resolve_asset "$os" "$arch" "$libc")
  check "$os/$arch${libc:+/$libc}" "(none)" "$actual"
done <<'UNSUPPORTED'
Linux|aarch64|musl
Linux|armv7l|gnu
Linux|riscv64|gnu
Darwin|ppc
FreeBSD|amd64
MINGW64_NT-10.0|x86_64
MSYS_NT-10.0|x86_64
CYGWIN_NT-10.0|x86_64
UNSUPPORTED

# --- 2. the round trip with release.yml --------------------------------------

echo "== release matrix round trip =="
# sed rather than a `case` inside the command substitution: bash 3.2, which is
# still what /bin/bash is on macOS, mis-parses a case pattern's `)` in there.
workflow_assets=$(
  sed -n 's/^ *- target: *\([A-Za-z0-9_.-]*\).*/\1/p' "$workflow" |
    sed -e 's/^/sheep-/' |
    sort -u
)
[ -n "$workflow_assets" ] || {
  echo "FAIL  no targets found in $workflow" >&2
  exit 1
}

detected_assets=$(
  {
    resolve_asset Darwin arm64 ""
    resolve_asset Darwin x86_64 ""
    resolve_asset Linux x86_64 gnu
    resolve_asset Linux x86_64 musl
    resolve_asset Linux aarch64 gnu
  } | sort -u
)

if [ "$workflow_assets" = "$detected_assets" ]; then
  printf 'ok    release.yml builds exactly the assets install.sh asks for:\n'
  printf '%s\n' "$workflow_assets" | sed 's|^|        |'
else
  echo "FAIL  release.yml and install.sh disagree about asset names" >&2
  echo "--- release.yml builds ---" >&2
  printf '%s\n' "$workflow_assets" >&2
  echo "--- install.sh asks for ---" >&2
  printf '%s\n' "$detected_assets" >&2
  failures=$((failures + 1))
fi

# --- 3. download, verify, install --------------------------------------------

# A fake release served over file:// so the fetch/checksum/install path runs
# end to end without a network or a published tag.
echo "== fetch, verify, install =="
version=$(sed -n 's/^version *= *"\([^"]*\)".*/\1/p' "$plugin_root/herdr-plugin.toml" | head -n 1)
sandbox=$(mktemp -d)
trap 'rm -rf "$sandbox"' EXIT
fake_release="$sandbox/release/v$version"
out_dir="$sandbox/bin"
mkdir -p "$fake_release" "$out_dir"

asset="sheep-x86_64-unknown-linux-musl"
printf '#!/bin/sh\necho "sheep %s"\n' "$version" >"$fake_release/$asset"
chmod +x "$fake_release/$asset"
if command -v sha256sum >/dev/null 2>&1; then
  (cd "$fake_release" && sha256sum "$asset" >SHA256SUMS)
else
  (cd "$fake_release" && shasum -a 256 "$asset" >SHA256SUMS)
fi

run_installer() {
  SHEEP_TEST_MODE=1 \
    SHEEP_TEST_UNAME_S=Linux \
    SHEEP_TEST_UNAME_M=x86_64 \
    SHEEP_TEST_LIBC=musl \
    SHEEP_TEST_BASE_URL="file://$sandbox/release" \
    SHEEP_TEST_OUT_DIR="$out_dir" \
    bash "$installer" "$@"
}

if run_installer >"$sandbox/install.log" 2>&1 && [ -x "$out_dir/sheep" ]; then
  printf 'ok    installs a verified asset (%s)\n' "$(cat "$sandbox/install.log")"
else
  echo "FAIL  a verified asset did not install" >&2
  cat "$sandbox/install.log" >&2
  failures=$((failures + 1))
fi

# Re-running must be a no-op, not a re-download: herdr calls this on every
# install and the event hooks lean on the same idempotence.
if run_installer 2>&1 | grep -q "already installed"; then
  printf 'ok    re-running leaves an up-to-date binary alone\n'
else
  echo "FAIL  re-running did not short-circuit on the installed version" >&2
  failures=$((failures + 1))
fi

# A corrupted checksum must refuse, and must leave the good binary in place.
printf '%064d  %s\n' 0 "$asset" >"$fake_release/SHA256SUMS"
if run_installer --force >"$sandbox/bad.log" 2>&1; then
  echo "FAIL  a checksum mismatch was installed anyway" >&2
  failures=$((failures + 1))
else
  if grep -q "checksum mismatch" "$sandbox/bad.log" && [ -x "$out_dir/sheep" ]; then
    printf 'ok    refuses a checksum mismatch and leaves the old binary in place\n'
  else
    echo "FAIL  a checksum mismatch failed for the wrong reason" >&2
    cat "$sandbox/bad.log" >&2
    failures=$((failures + 1))
  fi
fi

# --- 4. a staged release directory, when asked -------------------------------

if [ -n "$release_dir" ]; then
  echo "== staged release directory =="
  staged=$(find "$release_dir" -maxdepth 1 -type f ! -name 'SHA256SUMS' -exec basename {} \; | sort -u)
  if [ "$staged" = "$workflow_assets" ]; then
    printf 'ok    %s holds exactly the expected assets\n' "$release_dir"
  else
    echo "FAIL  $release_dir does not hold the expected assets" >&2
    echo "--- staged ---" >&2
    printf '%s\n' "$staged" >&2
    echo "--- expected ---" >&2
    printf '%s\n' "$workflow_assets" >&2
    failures=$((failures + 1))
  fi

  for name in $workflow_assets; do
    if grep -qE "^[0-9a-fA-F]{64} [ *]$name\$" "$release_dir/SHA256SUMS"; then
      printf 'ok    SHA256SUMS lists %s\n' "$name"
    else
      printf 'FAIL  SHA256SUMS does not list %s\n' "$name" >&2
      failures=$((failures + 1))
    fi
  done
fi

echo
if [ "$failures" -eq 0 ]; then
  echo "all checks passed"
  exit 0
fi
echo "$failures check(s) failed" >&2
exit 1
