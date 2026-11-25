#!/usr/bin/env bash
# Fills in the winget and Scoop templates next to this script for a release:
#
#   deploy/cli/render.sh cli-v0.3.0 artifacts/SHA256SUMS out/
#
# writes out/roll.json (Scoop: `scoop install <its release URL>`) and out/roll-winget.zip (the three
# winget manifests, for microsoft/winget-pkgs). The hashes are the release's own SHA256SUMS.
set -euo pipefail
tag=$1 sums=$2 out=$3
here=$(cd "$(dirname "$0")" && pwd)
version=${tag#cli-v}
[[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+([.-][0-9A-Za-z.-]+)?$ ]] || { echo "render.sh: $tag isn't cli-v<version>" >&2; exit 1; }
hash() {
  local h
  h=$(awk -v f="roll-$1-pc-windows-msvc.zip" '$2 == f || $2 == "*" f { print $1 }' "$sums")
  [[ $h =~ ^[0-9a-f]{64}$ ]] || { echo "render.sh: $sums has no checksum for roll-$1-pc-windows-msvc.zip" >&2; exit 1; }
  echo "$h"
}
x64=$(hash x86_64) arm64=$(hash aarch64)
fill() { sed -e "s/{{VERSION}}/$version/g" -e "s/{{TAG}}/$tag/g" -e "s/{{SHA256_X86_64}}/$x64/g" -e "s/{{SHA256_AARCH64}}/$arm64/g" "$1"; }
mkdir -p "$out/winget"
fill "$here/scoop/roll.json" > "$out/roll.json"
for f in "$here"/winget/*.yaml; do
  fill "$f" | grep -v '^#' > "$out/winget/$(basename "$f")"
done
if grep -rq '{{' "$out/roll.json" "$out/winget"; then
  echo "render.sh: a placeholder is left" >&2
  exit 1
fi
(cd "$out/winget" && python3 -I -m zipfile -c ../roll-winget.zip ./*.yaml)
rm -r "$out/winget"
