#!/usr/bin/env bash
# roll code agents test suite (docs/code/agents.md): build, clippy and the tests of agent-proto,
# agent-host (orchestrator, providers against recorded transcripts, worktrees, terminals, the MCP
# server, the relay link), the relay's tests in code-control (agents, code access, the auth
# matrix, MCP agent tools, the coverage map), `roll agent` against a live relay, and the
# dashboard's and desktop app's tests; then the install script against a local release, and an
# end to end run of the real binaries
# (scripts/agents-e2e.mjs): rolld --code-only, `roll agent login` approved through the API,
# `ROLL_AGENT_FAKE=1 roll agent up`, a streamed run with an approval, its checkpoint and diff, a
# share read anonymously, the dashboard in a production build serving the canvas and the share
# page, and `roll agent logout`. docs/code/test-coverage.md maps features to tests.
#
# Only runs when something agents-related changed since the last green run. `--force` runs
# regardless; `--unit` skips the install check and the end to end run (no dashboard build).
set -euo pipefail
cd "$(dirname "$0")/.."

PATHS=(crates/agent-proto crates/agent-host crates/code-control/src/agents crates/code-control/tests/agents.rs crates/code-control/tests/agents_code.rs
       crates/code-control/tests/common crates/code-control/tests/mcp.rs crates/code-control/tests/routes.rs
       crates/roll-cli/src/agent.rs crates/roll-cli/tests/agent.rs crates/roll-cli/Cargo.toml crates/rolld/src
       apps/dashboard/src apps/dashboard/public/install.sh apps/desktop/src apps/desktop/test apps/dashboard/package.json apps/dashboard/next.config.ts
       Cargo.toml Cargo.lock scripts/test-agents.sh scripts/agents-e2e.mjs)
TARGET=${CARGO_TARGET_DIR:-target}
STAMP=$TARGET/.test-agents.hash

force=0 unit_only=0
for a in "$@"; do
  case "$a" in
    --force) force=1 ;;
    --unit) unit_only=1 ;;
    *) echo "unknown argument $a" >&2; exit 2 ;;
  esac
done

hash=$(find "${PATHS[@]}" -type f -not -path '*/target/*' 2>/dev/null | LC_ALL=C sort | xargs shasum -a 256 | shasum -a 256 | cut -d' ' -f1)
if [[ $force == 0 && -f $STAMP && "$(cat "$STAMP")" == "$hash" ]]; then
  echo "agents: no changes since the last green run; skipping (--force to run anyway)"
  exit 0
fi

if ! git --version >/dev/null 2>&1; then
  echo "agents: the tests drive the real git client; install git" >&2
  exit 1
fi

CRATES=(-p agent-proto -p agent-host -p roll-cli)
cargo build "${CRATES[@]}" -p code-control -p rolld
cargo clippy "${CRATES[@]}" -p code-control --all-targets -- -D warnings
cargo test -p agent-proto
cargo test -p agent-host
# The relay: its own tests, the agents rows of the auth matrix, the MCP agent tools, the coverage map.
cargo test -p code-control --lib -- agents cluster::
cargo test -p code-control --test agents --test agents_code --test coverage_map
cargo test -p code-control --test routes agents
cargo test -p code-control --test mcp agent
cargo test -p roll-cli --test agent
cargo test -p roll-cli --bin roll coverage
pnpm --filter dashboard test
pnpm --filter @roll/desktop test

if [[ $unit_only == 0 ]]; then
  # The install script, against a release served from a local directory: the right asset for this
  # machine, a checksum that matches, and one that doesn't.
  work=$(mktemp -d "${TMPDIR:-/tmp}/roll-agents-install.XXXXXX")
  trap 'rm -rf "$work"' EXIT
  case "$(uname -s)-$(uname -m)" in
    Linux-x86_64) target=x86_64-unknown-linux-gnu ;;
    Linux-aarch64) target=aarch64-unknown-linux-gnu ;;
    Darwin-x86_64) target=x86_64-apple-darwin ;;
    Darwin-arm64) target=aarch64-apple-darwin ;;
    *) target="" ;;
  esac
  if [[ -n $target ]]; then
    mkdir -p "$work/rel/download/cli-v9.9.9" "$work/pkg"
    cp "$TARGET/debug/roll" "$work/pkg/roll"
    tar -C "$work/pkg" -czf "$work/rel/download/cli-v9.9.9/roll-$target.tar.gz" roll
    (cd "$work/rel/download/cli-v9.9.9" && shasum -a 256 "roll-$target.tar.gz" > SHA256SUMS)
    ROLL_RELEASES="file://$work/rel" ROLL_VERSION=9.9.9 ROLL_INSTALL_DIR="$work/bin" sh apps/dashboard/public/install.sh
    "$work/bin/roll" agent --help >/dev/null
    rm "$work/bin/roll"
    echo "0000000000000000000000000000000000000000000000000000000000000000  roll-$target.tar.gz" > "$work/rel/download/cli-v9.9.9/SHA256SUMS"
    if ROLL_RELEASES="file://$work/rel" ROLL_VERSION=cli-v9.9.9 ROLL_INSTALL_DIR="$work/bin" sh apps/dashboard/public/install.sh 2>"$work/err"; then
      echo "agents: install.sh installed a download that doesn't match its checksum" >&2
      exit 1
    fi
    grep -q "doesn't match its checksum" "$work/err"
    [[ ! -e $work/bin/roll ]]
    echo "agents: install.sh checks out"
  fi
  node scripts/agents-e2e.mjs "$TARGET/debug"
fi

# Only a full run counts as green.
if [[ $unit_only == 0 ]]; then
  mkdir -p "$TARGET" && echo "$hash" > "$STAMP"
fi
echo "agents: all green"
