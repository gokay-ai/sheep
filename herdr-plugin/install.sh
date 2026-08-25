#!/usr/bin/env bash
# Put a working `sheep` binary at <plugin root>/bin/sheep without needing Rust.
#
# herdr runs this as the plugin's [[build]] step on `herdr plugin install`, with
# cwd = the plugin root and with every HERDR_* runtime variable scrubbed, so the
# script resolves everything from its own location. It is also fine to run by
# hand, and re-running it is cheap: an already-installed binary of the right
# version is left alone.
#
#   ./install.sh                 fetch, verify and install the release binary
#   ./install.sh --dry-run       print what would be fetched; touch nothing
#   ./install.sh --from-source   build with cargo instead (needs Rust)
#   ./install.sh --force         reinstall even if the right version is present
#
# Exit status is 0 on success and non-zero with a one-line reason on stderr
# otherwise — herdr shows that line when an install fails.

set -eu

# The repository the release assets come from. Not configurable outside test
# mode on purpose: where the binary comes from is not something an environment
# variable should be able to redirect on a real install.
readonly SHEEP_GITHUB_REPO="gokay-ai/sheep"
readonly SHEEP_BINARY_NAME="sheep"

script_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
readonly script_dir
readonly plugin_root="$script_dir"
readonly repo_root="$script_dir/.."

# Test seams. Enabled only by SHEEP_TEST_MODE=1 so that platform detection and
# the download origin can be exercised without ever being redirectable on a
# user's machine. scripts/test-install.sh is the only intended caller.
test_mode=${SHEEP_TEST_MODE:-0}
if [ "$test_mode" = 1 ]; then
  uname_s=${SHEEP_TEST_UNAME_S:-$(uname -s 2>/dev/null || echo unknown)}
  uname_m=${SHEEP_TEST_UNAME_M:-$(uname -m 2>/dev/null || echo unknown)}
  libc_hint=${SHEEP_TEST_LIBC:-}
  base_url=${SHEEP_TEST_BASE_URL:-"https://github.com/$SHEEP_GITHUB_REPO/releases/download"}
  out_dir=${SHEEP_TEST_OUT_DIR:-"$plugin_root/bin"}
else
  uname_s=$(uname -s 2>/dev/null || echo unknown)
  uname_m=$(uname -m 2>/dev/null || echo unknown)
  libc_hint=""
  base_url="https://github.com/$SHEEP_GITHUB_REPO/releases/download"
  out_dir="$plugin_root/bin"
fi

dry_run=0
from_source=0
force=0

die() {
  echo "sheep: $*" >&2
  exit 1
}

usage() {
  sed -n '2,16p' "$0" | sed 's/^# \{0,1\}//'
}

while [ $# -gt 0 ]; do
  case "$1" in
    --dry-run) dry_run=1 ;;
    --from-source) from_source=1 ;;
    --force) force=1 ;;
    -h | --help)
      usage
      exit 0
      ;;
    *) die "unknown option '$1'. try --help" ;;
  esac
  shift
done

# SHEEP_FROM_SOURCE=1 is the escape hatch for `herdr plugin install`, which runs
# this script with a fixed argv and no way to pass --from-source.
if [ "${SHEEP_FROM_SOURCE:-0}" = 1 ]; then
  from_source=1
fi

# --- what platform is this, and which asset does it want ---------------------

# musl systems have no glibc, so a *-linux-gnu binary will not even start on
# them; glibc systems can run either but the gnu build is the smaller and
# faster-starting of the two. Get this wrong in the musl direction and the user
# sees "not found" from the loader, which reads like a missing file.
detect_libc() {
  if [ -n "$libc_hint" ]; then
    echo "$libc_hint"
    return 0
  fi
  if ldd --version 2>&1 | head -n 1 | grep -qi musl; then
    echo musl
    return 0
  fi
  # Alpine's ldd is a symlink into the loader and prints usage instead of a
  # version, so also look for the loader itself.
  for loader in /lib/ld-musl-*.so.1; do
    if [ -e "$loader" ]; then
      echo musl
      return 0
    fi
  done
  echo gnu
}

target_triple() {
  case "$uname_s" in
    Darwin)
      case "$uname_m" in
        arm64 | aarch64) echo aarch64-apple-darwin ;;
        x86_64 | amd64) echo x86_64-apple-darwin ;;
        *) echo "" ;;
      esac
      ;;
    Linux)
      libc=$(detect_libc)
      case "$uname_m/$libc" in
        x86_64/gnu | amd64/gnu) echo x86_64-unknown-linux-gnu ;;
        x86_64/musl | amd64/musl) echo x86_64-unknown-linux-musl ;;
        aarch64/gnu | arm64/gnu) echo aarch64-unknown-linux-gnu ;;
        *) echo "" ;;
      esac
      ;;
    # Git Bash / MSYS2 / Cygwin. The normal Windows path is install.ps1, but a
    # hand-run of this script from one of those shells should still work.
    MINGW* | MSYS* | CYGWIN*)
      case "$uname_m" in
        x86_64 | amd64) echo x86_64-pc-windows-msvc ;;
        *) echo "" ;;
      esac
      ;;
    *) echo "" ;;
  esac
}

triple=$(target_triple)

exe_suffix=""
case "$triple" in
  *-windows-*) exe_suffix=".exe" ;;
esac

# --- which version -----------------------------------------------------------

# The plugin manifest, not Cargo.toml: the manifest version is what herdr shows
# and what the release tag is cut from, and CI asserts the two agree. Reading it
# also keeps this script working when only the plugin subdirectory is present.
manifest="$plugin_root/herdr-plugin.toml"
version=$(sed -n 's/^version *= *"\([^"]*\)".*/\1/p' "$manifest" 2>/dev/null | head -n 1)
[ -n "$version" ] || die "could not read the plugin version from $manifest"

asset="$SHEEP_BINARY_NAME-$triple$exe_suffix"
out="$out_dir/$SHEEP_BINARY_NAME$exe_suffix"

if [ "$dry_run" = 1 ]; then
  echo "os          $uname_s"
  echo "arch        $uname_m"
  echo "version     $version"
  if [ -n "$triple" ]; then
    echo "target      $triple"
    echo "asset       $asset"
    echo "url         $base_url/v$version/$asset"
    echo "install to  $out"
  else
    echo "target      (none — no prebuilt binary for this platform)"
    echo "asset       (none)"
    echo "install to  $out"
  fi
  exit 0
fi

# --- source build ------------------------------------------------------------

build_from_source() {
  # rustup's shim directory is not on a non-login shell's PATH on some systems.
  if [ -f "$HOME/.cargo/env" ]; then
    # shellcheck disable=SC1091  # sourced from the user's machine, not this repo
    . "$HOME/.cargo/env"
  fi
  command -v cargo >/dev/null 2>&1 ||
    die "building from source needs Rust — install it from https://rustup.rs and retry"
  [ -f "$repo_root/Cargo.toml" ] ||
    die "building from source needs the full checkout; $repo_root/Cargo.toml is missing"

  echo "sheep: building v$version from source (this takes a few minutes)."
  (cd "$repo_root" && cargo build --release)

  built="$repo_root/target/release/$SHEEP_BINARY_NAME$exe_suffix"
  [ -f "$built" ] || die "cargo finished but $built does not exist"
  mkdir -p "$out_dir"
  cp -f "$built" "$out"
  chmod +x "$out"
  echo "sheep: installed a source build at $out."
}

if [ "$from_source" = 1 ]; then
  build_from_source
  exit 0
fi

[ -n "$triple" ] || die "no prebuilt sheep binary for $uname_s/$uname_m.
  Prebuilt platforms are macOS (arm64, x86_64), Linux (x86_64 gnu/musl, aarch64
  gnu) and Windows x86_64. Re-run as \`SHEEP_FROM_SOURCE=1 herdr plugin install\`
  (or \`./install.sh --from-source\`) to build it with cargo instead."

# --- already installed? ------------------------------------------------------

if [ "$force" != 1 ] && [ -x "$out" ]; then
  installed=$("$out" --version 2>/dev/null | awk 'NR == 1 { print $NF }')
  if [ "$installed" = "$version" ]; then
    echo "sheep: v$version is already installed at $out."
    exit 0
  fi
fi

# --- fetch and verify --------------------------------------------------------

tmpdir=""
cleanup() {
  [ -n "$tmpdir" ] && rm -rf "$tmpdir"
  return 0
}
trap cleanup EXIT

download() {
  name=$1
  dest=$2
  url="$base_url/v$version/$name"
  case "$url" in
    file://*) cp "${url#file://}" "$dest" ;;
    *)
      if command -v curl >/dev/null 2>&1; then
        curl --fail --silent --show-error --location --retry 3 --output "$dest" "$url"
      elif command -v wget >/dev/null 2>&1; then
        wget --quiet --tries=3 --output-document "$dest" "$url"
      else
        die "neither curl nor wget is available; cannot download $url"
      fi
      ;;
  esac
}

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{ print $1 }'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{ print $1 }'
  else
    die "no sha256sum or shasum available; refusing to install an unverified binary"
  fi
}

tmpdir=$(mktemp -d) || die "could not create a temporary directory"

download SHA256SUMS "$tmpdir/SHA256SUMS" ||
  die "release v$version has no SHA256SUMS. Is the release published?
  See https://github.com/$SHEEP_GITHUB_REPO/releases/tag/v$version"
download "$asset" "$tmpdir/$asset" ||
  die "release v$version has no asset named '$asset' for $uname_s/$uname_m.
  See https://github.com/$SHEEP_GITHUB_REPO/releases/tag/v$version"

# `sha256sum` writes "<hash>  <name>" and `shasum -b` writes "<hash> *<name>";
# accept either, and match the whole line so a substring of another asset name
# can never be picked up.
expected=$(awk -v want="$asset" '
  $1 ~ /^[0-9a-fA-F]{64}$/ {
    name = $2
    sub(/^\*/, "", name)
    if (name == want) { print tolower($1); exit }
  }' "$tmpdir/SHA256SUMS")
[ -n "$expected" ] || die "SHA256SUMS for v$version does not list '$asset'"

actual=$(sha256_of "$tmpdir/$asset")
[ "$actual" = "$expected" ] ||
  die "checksum mismatch for $asset (expected $expected, got $actual). Nothing was installed."

# --- install -----------------------------------------------------------------

mkdir -p "$out_dir" || die "could not create $out_dir"
chmod +x "$tmpdir/$asset"
# Replace rather than write in place: a running dock holds the old inode open
# and keeps working until it exits.
mv -f "$tmpdir/$asset" "$out" || die "could not install the verified binary at $out"

echo "sheep: installed verified v$version ($triple) at $out."
