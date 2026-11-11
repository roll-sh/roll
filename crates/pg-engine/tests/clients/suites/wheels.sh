#!/bin/sh
# python3 -m venv may be missing (Debian's python3-venv): psycopg's wheels from PyPI into $1.
# wheels.sh <dir> <python tag, e.g. cp311>
set -eu
D=$1; TAG=$2
[ -f "$D/.done" ] && exit 0
mkdir -p "$D"
python3 - "$D" "$TAG" <<'EOF'
import io, json, sys, urllib.request, zipfile
d, tag = sys.argv[1], sys.argv[2]
want = [("psycopg", "3.2.9", lambda f: f.endswith("py3-none-any.whl")),
        ("psycopg-binary", "3.2.9", lambda f: tag in f and "manylinux" in f and "x86_64" in f),
        ("typing-extensions", "4.15.0", lambda f: f.endswith("py3-none-any.whl"))]
for name, ver, ok in want:
    meta = json.load(urllib.request.urlopen("https://pypi.org/pypi/%s/%s/json" % (name, ver)))
    url = next(u["url"] for u in meta["urls"] if ok(u["filename"]))
    zipfile.ZipFile(io.BytesIO(urllib.request.urlopen(url).read())).extractall(d)
EOF
touch "$D/.done"
