#!/bin/sh
# Install the roll CLI (`roll`, which brings `roll agent`) from the GitHub releases of roll-sh/roll:
#
#   curl -fsSL https://roll.sh/install.sh | sh
#
# It picks the build for this OS and CPU, checks it against the release's SHA256SUMS, and puts
# `roll` in ~/.local/bin. With `--with-rolld` (or ROLL_INSTALL_ROLLD=1) it puts the roll daemon,
# `rolld`, next to it too (Linux and macOS), checked the same way; never without one of them:
#
#   curl -fsSL https://roll.sh/install.sh | sh -s -- --with-rolld
#
# Settings (environment):
#   ROLL_VERSION        a release tag (`cli-v0.2.0`) or version (`0.2.0`); default: the newest cli-v* release
#   ROLL_INSTALL_DIR    where to put `roll` (and `rolld`) (default: ~/.local/bin)
#   ROLL_INSTALL_ROLLD  1: install `rolld` too, as `--with-rolld` does
#   ROLL_RELEASES       where releases are downloaded from (default: https://github.com/roll-sh/roll/releases)
set -eu

REPO=roll-sh/roll
RELEASES=${ROLL_RELEASES:-https://github.com/$REPO/releases}
DIR=${ROLL_INSTALL_DIR:-$HOME/.local/bin}

say() { printf 'roll: %s\n' "$*" >&2; }
die() { say "$*"; exit 1; }

rolld=${ROLL_INSTALL_ROLLD:-0}
case "$rolld" in
  0 | "") rolld=0 ;;
  1) ;;
  *) die "ROLL_INSTALL_ROLLD is 1 (install rolld too) or 0, not $rolld" ;;
esac
for a in "$@"; do
  case "$a" in
    --with-rolld) rolld=1 ;;
    *) die "unknown option $a (the one option is --with-rolld)" ;;
  esac
done

fetch() { # url → file
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL --retry 3 -o "$2" "$1"
  elif command -v wget >/dev/null 2>&1; then
    wget -q -O "$2" "$1"
  else
    die "needs curl or wget"
  fi
}

case "$(uname -s)" in
  Linux) os=unknown-linux-gnu ;;
  Darwin) os=apple-darwin ;;
  *) die "no build for $(uname -s) yet: build it from source (cargo install --git https://github.com/$REPO roll-cli)" ;;
esac
case "$(uname -m)" in
  x86_64 | amd64) arch=x86_64 ;;
  aarch64 | arm64) arch=aarch64 ;;
  *) die "no build for $(uname -m) yet" ;;
esac
# A Rosetta shell on Apple silicon reports x86_64: take the native build.
if [ "$os" = apple-darwin ] && [ "$arch" = x86_64 ] && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || echo 0)" = 1 ]; then
  arch=aarch64
fi
target=$arch-$os

tmp=$(mktemp -d 2>/dev/null || mktemp -d -t roll)
trap 'rm -rf "$tmp"' EXIT INT TERM

tag=${ROLL_VERSION:-}
case "$tag" in
  "") ;;
  cli-v*) ;;
  v*) tag=cli-$tag ;;
  *) tag=cli-v$tag ;;
esac
if [ -z "$tag" ]; then
  fetch "https://api.github.com/repos/$REPO/releases?per_page=50" "$tmp/releases.json" || die "couldn't list the releases of $REPO"
  tag=$(grep -o '"tag_name": *"cli-v[^"]*"' "$tmp/releases.json" | head -n 1 | sed 's/.*"\(cli-v[^"]*\)"$/\1/')
  [ -n "$tag" ] || die "no cli-v* release of $REPO yet"
fi

bins=roll
[ "$rolld" = 0 ] || bins="roll rolld"
say "installing $tag ($bins) for $target into $DIR"
fetch "$RELEASES/download/$tag/SHA256SUMS" "$tmp/SHA256SUMS" || die "couldn't download the checksums of $tag"

# Downloads `<bin>-<target>.tar.gz`, checks it against SHA256SUMS and unpacks its one binary into
# $tmp/<bin>/. Every download is checked before anything is installed.
unpack() {
  asset=$1-$target.tar.gz
  want=$(awk -v f="$asset" '$2 == f || $2 == "*" f { print $1 }' "$tmp/SHA256SUMS")
  [ -n "$want" ] || die "$tag has no $asset (SHA256SUMS doesn't list it)"
  fetch "$RELEASES/download/$tag/$asset" "$tmp/$asset" || die "couldn't download $asset of $tag"
  if command -v sha256sum >/dev/null 2>&1; then
    have=$(sha256sum "$tmp/$asset" | awk '{ print $1 }')
  elif command -v shasum >/dev/null 2>&1; then
    have=$(shasum -a 256 "$tmp/$asset" | awk '{ print $1 }')
  else
    die "needs sha256sum or shasum to check the download"
  fi
  [ "$have" = "$want" ] || die "$asset doesn't match its checksum (got $have, want $want): not installing it"
  mkdir "$tmp/$1"
  tar -xzf "$tmp/$asset" -C "$tmp/$1"
  [ -f "$tmp/$1/$1" ] || die "$asset has no $1 binary"
  chmod 755 "$tmp/$1/$1"
}
for b in $bins; do unpack "$b"; done

mkdir -p "$DIR"
for b in $bins; do
  # Moved into place in one step, so a running `roll agent up` (or rolld) keeps its old binary
  # until it restarts.
  mv -f "$tmp/$b/$b" "$DIR/$b.new"
  mv -f "$DIR/$b.new" "$DIR/$b"
  say "installed $("$DIR/$b" --version 2>/dev/null || echo "$b")"
done

case ":$PATH:" in
  *":$DIR:"*) ;;
  *) say "$DIR isn't on your PATH: add  export PATH=\"$DIR:\$PATH\"  to your shell's profile" ;;
esac
if [ "$rolld" = 0 ]; then
  say "next: roll agent login, then roll agent up"
else
  say "next: start rolld as https://github.com/$REPO/blob/main/docs/secrets/getting-started.md (step 0) shows"
fi
