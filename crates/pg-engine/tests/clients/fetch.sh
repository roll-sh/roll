#!/bin/sh
# fetch.sh <dir> <tarball url>: download and unpack a pinned source tarball once.
set -eu
dir=$1 url=$2
[ -f "$dir/.fetched" ] && grep -qxF "$url" "$dir/.fetched" && exit 0
rm -rf "$dir" && mkdir -p "$dir"
curl -fsSL "$url" | tar xz -C "$dir" --strip-components=1
echo "$url" > "$dir/.fetched"
