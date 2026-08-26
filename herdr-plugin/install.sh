#!/usr/bin/env bash
# Put a working `sheep` binary at <plugin root>/bin/sheep without needing Rust,
# a copy on PATH so `sheep doctor` is a real shell command afterwards, and
# Sheep's keys into herdr's config.toml so prefix+F / prefix+f work without a
# paste step. herdr 0.8 ignores [[keys.command]] in a plugin manifest.
#
# herdr runs this as the plugin's [[build]] step on `herdr plugin install`, with
# cwd = the plugin root and with every HERDR_* runtime variable scrubbed, so the
# script resolves everything from its own location. It is also fine to run by
# hand, and re-running it is cheap: an already-installed binary of the right
# version is left alone. The PATH copy and the keybindings are still refreshed
# on that path: a plugin that is already in place with no `sheep` on PATH, and
# no keys in config.toml, is exactly the shape that made the advertised
# commands fail after a successful install.
#
#   ./install.sh                 fetch, verify and install the release binary
#   ./install.sh --dry-run       print what would be fetched; touch nothing
#   ./install.sh --from-source   build with cargo instead (needs Rust)
#   ./install.sh --force         reinstall even if the right version is present
#   ./install.sh --no-path       do not copy sheep onto PATH (~/.local/bin)
#   ./install.sh --no-keys       do not write Sheep's keys into herdr's config.toml
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
skip_path=0
skip_keys=0

die() {
  echo "sheep: $*" >&2
  exit 1
}

usage() {
  sed -n '2,24p' "$0" | sed 's/^# \{0,1\}//'
}

while [ $# -gt 0 ]; do
  case "$1" in
    --dry-run) dry_run=1 ;;
    --from-source) from_source=1 ;;
    --force) force=1 ;;
    --no-path) skip_path=1 ;;
    --no-keys) skip_keys=1 ;;
    -h | --help)
      usage
      exit 0
      ;;
    *) die "unknown option '$1'. try --help" ;;
  esac
  shift
done

# SHEEP_FROM_SOURCE=1 is the escape hatch for `herdr plugin install`, which runs
# this script with a fixed argv and no way to pass --from-source. SHEEP_SKIP_PATH
# and SHEEP_SKIP_KEYS are the same shape for --no-path and --no-keys.
if [ "${SHEEP_FROM_SOURCE:-0}" = 1 ]; then
  from_source=1
fi
if [ "${SHEEP_SKIP_PATH:-0}" = 1 ]; then
  skip_path=1
fi
if [ "${SHEEP_SKIP_KEYS:-0}" = 1 ]; then
  skip_keys=1
fi

# User-facing `sheep` on PATH. Empty means do not install one: test mode unless
# SHEEP_TEST_PATH_DIR is set (so test-install.sh cannot plant a binary in the
# developer's ~/.local/bin), --no-path, or a machine with no HOME.
path_dir=""
if [ "$skip_path" != 1 ]; then
  if [ "$test_mode" = 1 ]; then
    path_dir=${SHEEP_TEST_PATH_DIR:-}
  elif [ -n "${XDG_BIN_HOME:-}" ]; then
    path_dir=$XDG_BIN_HOME
  elif [ -n "${HOME:-}" ]; then
    path_dir=$HOME/.local/bin
  fi
fi

# herdr's config.toml. Empty means do not write keys: test mode unless
# SHEEP_TEST_CONFIG_PATH is set (so test-install.sh cannot edit the
# developer's herdr config), --no-keys, or a machine with no HOME.
# HERDR_CONFIG_PATH is honoured when present; plugin build scrubs HERDR_*
# so a real `herdr plugin install` falls through to the XDG path.
config_path=""
if [ "$skip_keys" != 1 ]; then
  if [ "$test_mode" = 1 ]; then
    config_path=${SHEEP_TEST_CONFIG_PATH:-}
  elif [ -n "${HERDR_CONFIG_PATH:-}" ]; then
    config_path=$HERDR_CONFIG_PATH
  elif [ -n "${XDG_CONFIG_HOME:-}" ]; then
    config_path=$XDG_CONFIG_HOME/herdr/config.toml
  elif [ -n "${HOME:-}" ]; then
    config_path=$HOME/.config/herdr/config.toml
  fi
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
    # Git Bash / MSYS2 / Cygwin used to resolve a windows-msvc asset here.
    # Sheep no longer builds one: the recorder is herdr's session API and there
    # is no non-unix transport for it, so a Windows binary could not record a
    # turn. Falling through to "no prebuilt binary" says that plainly instead of
    # installing something that starts and then does nothing.
    *) echo "" ;;
  esac
}

triple=$(target_triple)

# Every target Sheep ships is unix, so there is no executable suffix to add.
# Kept as a variable rather than deleted because the source-build path below and
# the asset name both interpolate it, and one of them will need it again the day
# a Windows target comes back.
exe_suffix=""

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
  if [ -n "$path_dir" ]; then
    echo "on PATH     $path_dir/$SHEEP_BINARY_NAME$exe_suffix"
  else
    echo "on PATH     (none)"
  fi
  if [ -n "$config_path" ]; then
    echo "keys        $config_path"
  else
    echo "keys        (none)"
  fi
  exit 0
fi

# --- PATH command ------------------------------------------------------------

# Copy the plugin binary onto the user's PATH so `sheep doctor` works after a
# herdr plugin install. The plugin root is not on PATH; that is the whole
# problem this exists to close.
#
# A copy, not a symlink. `herdr plugin uninstall` deletes the plugin tree, and
# a dangling `~/.local/bin/sheep` still wins `command -v` and then fails with
# "No such file or directory", which reads like a broken install. A copy
# survives uninstall, which is also how `sheep doctor` can still print the
# state directory the README tells you to delete. The copy is replaced
# whenever this script installs a sheep of a different version.
#
# Never overwrite a file that is not sheep. Never fail the plugin install if
# the PATH copy cannot be written — the dock and the recorder only need $out.
install_on_path() {
  if [ -z "$path_dir" ] || [ ! -x "$out" ]; then
    return 0
  fi

  path_cmd="$path_dir/$SHEEP_BINARY_NAME$exe_suffix"

  if [ -d "$path_cmd" ] && [ ! -L "$path_cmd" ]; then
    echo "sheep: $path_cmd is a directory; leaving it. The binary is at $out."
    return 0
  fi

  if [ -e "$path_cmd" ] || [ -L "$path_cmd" ]; then
    if [ -L "$path_cmd" ] && [ ! -e "$path_cmd" ]; then
      : # dangling; replace
    elif [ -x "$path_cmd" ]; then
      existing_line=$("$path_cmd" --version 2>/dev/null | awk 'NR == 1 { print }')
      existing_name=$(printf '%s\n' "$existing_line" | awk '{ print $1 }')
      existing_ver=$(printf '%s\n' "$existing_line" | awk '{ print $NF }')
      if [ "$existing_name" != "$SHEEP_BINARY_NAME" ]; then
        echo "sheep: $path_cmd exists and is not sheep; not overwriting. The binary is at $out."
        return 0
      fi
      if [ "$existing_ver" = "$version" ]; then
        echo "sheep: command is at $path_cmd."
        hint_if_path_dir_missing
        return 0
      fi
    else
      echo "sheep: $path_cmd exists and is not sheep; not overwriting. The binary is at $out."
      return 0
    fi
  fi

  if ! mkdir -p "$path_dir" 2>/dev/null; then
    echo "sheep: could not create $path_dir; run $out directly, or copy it onto PATH."
    return 0
  fi
  # Drop a leftover symlink so cp cannot write through it into the plugin tree.
  if [ -L "$path_cmd" ]; then
    rm -f "$path_cmd"
  fi
  if ! cp -f "$out" "$path_cmd" 2>/dev/null; then
    echo "sheep: could not write $path_cmd; run $out directly, or copy it onto PATH."
    return 0
  fi
  chmod +x "$path_cmd" 2>/dev/null || true
  echo "sheep: command is at $path_cmd."
  hint_if_path_dir_missing
}

hint_if_path_dir_missing() {
  case ":$PATH:" in
    *":$path_dir:"*) return 0 ;;
  esac
  echo "sheep: $path_dir is not on PATH. Add it and open a new terminal, then sheep doctor will work."
}

# True when an uncommented line of $1 contains $2. Comments are not bindings.
config_contains() {
  file=$1
  needle=$2
  [ -f "$file" ] || return 1
  awk -v n="$needle" '
    /^[[:space:]]*#/ { next }
    index($0, n) { found = 1; exit }
    END { exit found ? 0 : 1 }
  ' "$file"
}

# Write prefix+f / prefix+F into herdr's config.toml. herdr 0.8 reads
# keybindings from that file alone: the identical [[keys.command]] tables in
# herdr-plugin.toml are accepted by the manifest loader and never reach the
# key map (herdrdev/herdr#1368). That is why a successful plugin install
# still left the advertised keys doing nothing until the user pasted them.
#
# prefix+f is not a herdr default (zoom stays on prefix+z). The sheep-keys
# block is ours: if an older install wrote prefix+z here, a re-run replaces
# that block rather than leaving a zoom collision in place. A command that is
# not ours and already holds prefix+f is left alone. Back up first, and if
# `herdr config check` rejects the result, put the backup back. A failure
# here does not fail the plugin install — the dock and the recorder do not
# need the keys. Test mode writes only when SHEEP_TEST_CONFIG_PATH is set.
sheep_keys_are_current() {
  file=$1
  config_contains "$file" 'command = "sheep.rewind"' || return 1
  config_contains "$file" 'command = "sheep.dock"' || return 1
  config_contains "$file" 'key = "prefix+f"' || return 1
  config_contains "$file" 'key = "prefix+F"' || config_contains "$file" 'key = "prefix+shift+f"'
}

sheep_key_is_taken() {
  file=$1
  config_contains "$file" 'key = "prefix+f"' ||
    config_contains "$file" 'key = "prefix+F"' ||
    config_contains "$file" 'key = "prefix+shift+f"'
}

install_keybindings() {
  if [ -z "$config_path" ]; then
    return 0
  fi

  keys_file="$plugin_root/keybindings.toml"
  [ -f "$keys_file" ] || {
    echo "sheep: $keys_file is missing; not writing keybindings."
    return 0
  }

  if [ -d "$config_path" ]; then
    echo "sheep: $config_path is a directory; not writing keybindings."
    return 0
  fi

  mode=create
  if [ -f "$config_path" ]; then
    if sheep_keys_are_current "$config_path"; then
      echo "sheep: keys already in $config_path."
      return 0
    fi
    if grep -q '# --- sheep-keys ---' "$config_path"; then
      if sheep_key_is_taken "$config_path"; then
        echo "sheep: prefix+f or prefix+F is already bound in $config_path; not overwriting."
        return 0
      fi
      mode=replace
    elif config_contains "$config_path" 'command = "sheep.rewind"' &&
      config_contains "$config_path" 'command = "sheep.dock"'; then
      echo "sheep: keys already in $config_path."
      return 0
    elif sheep_key_is_taken "$config_path"; then
      echo "sheep: prefix+f or prefix+F is already bound in $config_path; not overwriting."
      return 0
    else
      mode=append
    fi
  fi

  config_dir=$(dirname -- "$config_path")
  if ! mkdir -p "$config_dir" 2>/dev/null; then
    echo "sheep: could not create $config_dir; bind prefix+F yourself from $keys_file."
    return 0
  fi

  new_file="$config_path.sheep-new"
  backup=""
  if [ -f "$config_path" ]; then
    backup="$config_path.sheep-bak"
    if ! cp -f "$config_path" "$backup" 2>/dev/null; then
      echo "sheep: could not back up $config_path; not writing keybindings."
      return 0
    fi
  fi

  case "$mode" in
    create)
      if ! cp -f "$keys_file" "$new_file" 2>/dev/null; then
        echo "sheep: could not write $config_path; bind prefix+F yourself from $keys_file."
        return 0
      fi
      ;;
    append)
      if ! {
        cat "$config_path"
        printf '\n'
        cat "$keys_file"
        printf '\n'
      } >"$new_file" 2>/dev/null; then
        echo "sheep: could not write $config_path; not changing keybindings."
        rm -f "$new_file"
        return 0
      fi
      ;;
    replace)
      # The marked block is ours. Drop it and write the current one, so an
      # older prefix+z install does not keep stealing herdr's zoom.
      if ! {
        awk '/^# --- sheep-keys ---$/ { exit } { print }' "$config_path"
        cat "$keys_file"
        printf '\n'
      } >"$new_file" 2>/dev/null; then
        echo "sheep: could not write $config_path; not changing keybindings."
        rm -f "$new_file"
        return 0
      fi
      ;;
    *)
      echo "sheep: internal error: unknown keybinding mode '$mode'."
      rm -f "$new_file"
      return 0
      ;;
  esac

  if ! mv -f "$new_file" "$config_path" 2>/dev/null; then
    echo "sheep: could not replace $config_path; not changing keybindings."
    rm -f "$new_file"
    return 0
  fi

  if [ "$test_mode" != 1 ] && command -v herdr >/dev/null 2>&1; then
    if ! herdr config check >/dev/null 2>&1; then
      echo "sheep: herdr rejected the keybinding edit; restored $config_path."
      if [ -n "$backup" ] && [ -f "$backup" ]; then
        mv -f "$backup" "$config_path" 2>/dev/null || true
      else
        rm -f "$config_path"
      fi
      return 0
    fi
    herdr server reload-config >/dev/null 2>&1 || true
  fi

  echo "sheep: bound prefix+f (rewind) and prefix+F (dock) in $config_path."
}

finish_install() {
  install_on_path
  install_keybindings
}

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
  finish_install
}

if [ "$from_source" = 1 ]; then
  build_from_source
  exit 0
fi

[ -n "$triple" ] || die "no prebuilt sheep binary for $uname_s/$uname_m.
  Prebuilt platforms are macOS (arm64, x86_64) and Linux (x86_64 gnu/musl,
  aarch64 gnu). Windows is not one of them: Sheep's recorder needs herdr's unix
  socket API and has no transport for it there yet. Re-run as
  \`SHEEP_FROM_SOURCE=1 herdr plugin install\` (or \`./install.sh --from-source\`)
  to build it with cargo instead."

# --- already installed? ------------------------------------------------------

if [ "$force" != 1 ] && [ -x "$out" ]; then
  installed=$("$out" --version 2>/dev/null | awk 'NR == 1 { print $NF }')
  if [ "$installed" = "$version" ]; then
    echo "sheep: v$version is already installed at $out."
    finish_install
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
finish_install
