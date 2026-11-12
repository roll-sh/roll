#!/usr/bin/env bash
# Local rehearsal of a first AWS deployment of roll search, without an AWS account.
#
#   scripts/search-aws-rehearsal.sh [userdata] [model] [localstack]      (default: all three)
#
#   userdata    the node instances' user data (NodeLaunchTemplate) run on Amazon Linux 2023 in
#               a privileged amazonlinux:2023 container against loop devices
#               (tests/search/aws/userdata-test.sh);
#   model       the template's scaling and capacity rules evaluated with AWS's documented
#               semantics: step adjustment bounds, the idle and scale-in alarms over simulated
#               minutes (scale to zero, a wake that must not be undone), deploy headroom with
#               distinctInstance, the node's cache and stop budgets (tests/search/aws/model.py,
#               in Python with PyYAML);
#   localstack  the stack deployed to LocalStack (community edition) as far as it implements the
#               resources: the full template first (the unsupported resource types are listed),
#               then every supported resource, and the Lambda front door invoked through its
#               function URL running the real arm64 image (search-proxy --lambda, under qemu)
#               in front of a proxy and a node from the image (tests/search/aws/localstack.py).
#
# Env: IMAGE (roll-search; the localstack part needs IMAGE:arm64 and IMAGE:amd64), LOCALSTACK_PORT
# (default 19284; also uses the next two ports), SCRATCH (default ~/.cache/roll-search/rehearsal).
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$PWD
TEMPLATE=deploy/search/aws/roll-search.yaml
SCRATCH=${SCRATCH:-$HOME/.cache/roll-search/rehearsal}/$$
PYIMAGE=roll-aws-rehearsal-py

docker_() {
  if docker info >/dev/null 2>&1; then docker "$@"; else sg docker -c "$(printf '%q ' docker "$@")"; fi
}

parts=()
for a in "$@"; do
  case "$a" in
    userdata | model | localstack) parts+=("$a") ;;
    *) echo "unknown argument $a" >&2; exit 2 ;;
  esac
done
[[ ${#parts[@]} -gt 0 ]] || parts=(userdata model localstack)
mkdir -p "$SCRATCH"
trap 'rm -rf "$SCRATCH"' EXIT

# Python with PyYAML (and boto3 for LocalStack), built once.
python_image() {
  if [[ -z $(docker_ images -q "$PYIMAGE") ]]; then
    printf '%s\n' 'FROM python:3.13-slim' 'RUN pip install -q --root-user-action=ignore pyyaml==6.0.3 boto3==1.40.55' |
      docker_ build -q -t "$PYIMAGE" - >/dev/null
  fi
}

userdata() {
  # The user data as CloudFormation renders it (!Sub with the cluster name).
  python3 - "$TEMPLATE" "$SCRATCH/userdata.sh" <<'EOF'
import sys
lines = open(sys.argv[1]).read().splitlines()
i = next(n for n, l in enumerate(lines) if l.strip() == "NodeLaunchTemplate:")
i = next(n for n in range(i, len(lines)) if lines[n].strip() == "Fn::Base64: !Sub |") + 1
indent = len(lines[i]) - len(lines[i].lstrip())
out = []
for l in lines[i:]:
    if l.strip() and len(l) - len(l.lstrip()) < indent:
        break
    out.append(l[indent:])
script = "\n".join(out).rstrip() + "\n"
assert script.startswith("#!/bin/bash\n"), script[:40]
script = script.replace("${Cluster}", "roll-test-cluster")
assert "${" not in script, "a !Sub variable the test doesn't know"
open(sys.argv[2], "w").write(script)
EOF
  bash -n "$SCRATCH/userdata.sh"
  docker_ run --rm --privileged -m 1g -v "$ROOT/tests/search/aws:/t:ro" -v "$SCRATCH:/w" amazonlinux:2023 \
    bash /t/userdata-test.sh /w/userdata.sh /w
}

model() {
  python_image
  docker_ run --rm -m 512m -v "$ROOT:/r:ro" -w /r "$PYIMAGE" python3 -I tests/search/aws/model.py "$TEMPLATE"
}

localstack() {
  python_image
  IMAGE=${IMAGE:-roll-search} LOCALSTACK_PORT=${LOCALSTACK_PORT:-19284} SCRATCH="$SCRATCH" \
    PYIMAGE="$PYIMAGE" bash tests/search/aws/localstack.sh
}

for p in "${parts[@]}"; do
  echo "search-aws-rehearsal: $p"
  "$p"
done
echo "search-aws-rehearsal: all green (${parts[*]})"
