#!/usr/bin/env bash
# roll code agents' local gate (docs/code/agents.md; docs/code/test-coverage.md maps features to
# tests): clippy and the tests of agent-proto, agent-host (orchestrator, providers against recorded
# transcripts, worktrees, terminals, the MCP server, the relay link), the relay's tests in
# code-control (agents, code access, the auth matrix, MCP agent tools, the coverage map), roll's
# model gateway (model-gateway, the relay's gateway routes, `roll gateway`), `roll agent` against a
# live relay, then typecheck and the dashboard's and desktop app's tests (scripts/lib/gate.sh);
# then the install script against a local release and an end to end run of the real binaries
# (scripts/agents-e2e.mjs): rolld --code-only, `roll agent login` approved through the API,
# `ROLL_AGENT_FAKE=1 roll agent up`, a streamed run with an approval, its checkpoint and diff, a
# share read anonymously, orchestration, and `roll agent logout`; when the dashboard changed, also
# the dashboard in a production build serving the canvas and the share page.
#
# Each part runs only when something it covers changed since its last green run: the Rust tests,
# the web tests, the binaries' end to end run, and its dashboard half. `--force` runs everything;
# `--unit` skips the install check and the end to end run.
set -euo pipefail
cd "$(dirname "$0")/.."
source scripts/lib/gate.sh

RUST_PATHS=(crates/agent-proto crates/agent-host crates/model-gateway crates/code-control/src/agents
            crates/code-control/tests/{agents,agents_code,agents_gateway,automations,linear,mcp,routes,coverage_map}.rs crates/code-control/tests/common
            crates/roll-cli/src/agent.rs crates/roll-cli/tests/agent.rs crates/roll-cli/src/gateway.rs crates/roll-cli/tests/gateway.rs crates/roll-cli/Cargo.toml
            docs/code/test-coverage.md Cargo.toml Cargo.lock scripts/test-agents.sh scripts/lib/gate.sh)
E2E_PATHS=("${RUST_PATHS[@]}" crates/code-control/src crates/rolld/src apps/dashboard/public/install.sh scripts/agents-e2e.mjs)
DASH_PATHS=(apps/dashboard/src apps/dashboard/public apps/dashboard/package.json apps/dashboard/vite.config.ts apps/dashboard/server.mjs apps/dashboard/tsconfig.json
            packages/brand/src pnpm-lock.yaml scripts/agents-e2e.mjs)

unit_only=0
for a in "$@"; do
  case "$a" in
    --force) gate_force=1 ;;
    --unit) unit_only=1 ;;
    *) echo "agents: unknown argument $a" >&2; exit 2 ;;
  esac
done
need_git agents

# Which test binaries are the agents' (the build has every lib in the package set; code-control's
# and roll-cli's are filtered to their agents parts; same-named targets elsewhere are skipped).
test_args() {
  case "$1:$2:$3" in
    agent-proto:* | agent-host:* | model-gateway:*) ;;
    code-control:code_control:lib) ARGS=(agents cluster::) ;;
    code-control:routes:test) ARGS=(agents) ;;
    code-control:mcp:test) ARGS=(agent) ;;
    code-control:agents:test | code-control:agents_code:test | code-control:agents_gateway:test | code-control:automations:test | code-control:linear:test | code-control:coverage_map:test) ;;
    roll-cli:roll:bin) ARGS=(coverage) ;;
    roll-cli:agent:test | roll-cli:gateway:test) ;;
    *) return 1 ;;
  esac
}

rust_hash=$(hash_of "${RUST_PATHS[@]}")
if stale "$TARGET/.test-agents-rust.hash" "$rust_hash"; then
  targets=(--lib --bin roll)
  for f in crates/{agent-proto,agent-host,model-gateway}/tests/*.rs; do targets+=(--test "$(basename "$f" .rs)"); done
  targets+=(--test agents --test agents_code --test agents_gateway --test automations --test linear --test coverage_map --test routes --test mcp --test agent --test gateway)
  rust_tests agents "${targets[@]}"
  echo "$rust_hash" > "$TARGET/.test-agents-rust.hash"
else
  echo "agents: Rust unchanged since its last green run; skipping it (--force to run anyway)"
fi

web_tests agents

[[ $unit_only == 1 ]] && { echo "agents: unit tests green"; exit 0; }

e2e_hash=$(hash_of "${E2E_PATHS[@]}")
dash_hash=$(hash_of "${DASH_PATHS[@]}")
e2e=0 dash=0
stale "$TARGET/.test-agents.hash" "$e2e_hash" && e2e=1
stale "$TARGET/.test-agents-dashboard.hash" "$dash_hash" && dash=1
if [[ $e2e == 0 && $dash == 0 ]]; then
  echo "agents: no changes since the last green end to end run; skipping it (--force to run anyway)"
  exit 0
fi
# roll and rolld as the integration tests build them (target/debug), so they share the package
# set's artifacts instead of a `cargo build` with its own features.
cargo test --no-run "${PKGS[@]}" --test agent --test code_cluster_e2e >/dev/null

if [[ $e2e == 1 ]]; then
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
    # Stripped: gzipping the 450 MB debug binary took most of this check's 19 s.
    strip -o "$work/pkg/roll" "$TARGET/debug/roll" 2>/dev/null || cp "$TARGET/debug/roll" "$work/pkg/roll"
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
fi

s=$(date +%s)
if [[ $dash == 1 ]]; then
  node scripts/agents-e2e.mjs "$TARGET/debug"
else
  echo "agents: the dashboard is unchanged since its last green end to end run; leaving out its build (--force to include it)"
  node scripts/agents-e2e.mjs "$TARGET/debug" --no-dashboard
fi
echo "agents: end to end run took $(( $(date +%s) - s ))s"

echo "$e2e_hash" > "$TARGET/.test-agents.hash"
[[ $dash == 1 ]] && echo "$dash_hash" > "$TARGET/.test-agents-dashboard.hash"
echo "agents: all green"
