#!/usr/bin/env bash
# roll code agents test suite (docs/code/agents.md): clippy and the tests of agent-proto,
# agent-host (orchestrator, providers against recorded transcripts, worktrees, terminals, the MCP
# server, the relay link), the relay's tests in code-control (agents, code access, the auth
# matrix, MCP agent tools, the coverage map), roll's model gateway (model-gateway, the relay's
# gateway routes, `roll gateway`), `roll agent` against a live relay, and the
# dashboard's and desktop app's tests; then the install script against a local release, and an
# end to end run of the real binaries
# (scripts/agents-e2e.mjs): rolld --code-only, `roll agent login` approved through the API,
# `ROLL_AGENT_FAKE=1 roll agent up`, a streamed run with an approval, its checkpoint and diff, a
# share read anonymously, the dashboard in a production build serving the canvas and the share
# page, and `roll agent logout`. docs/code/test-coverage.md maps features to tests.
#
# Each part runs only when something it covers changed since its last green run: the Rust tests,
# the web tests (dashboard and desktop), and the end to end run (anything). `--force` runs
# everything; `--unit` skips the install check and the end to end run (no dashboard build).
set -euo pipefail
cd "$(dirname "$0")/.."

RUST_PATHS=(crates/agent-proto crates/agent-host crates/model-gateway crates/code-control/src/agents
            crates/code-control/tests/{agents,agents_code,agents_gateway,automations,linear,mcp,routes,coverage_map}.rs crates/code-control/tests/common
            crates/roll-cli/src/agent.rs crates/roll-cli/tests/agent.rs crates/roll-cli/src/gateway.rs crates/roll-cli/tests/gateway.rs crates/roll-cli/Cargo.toml
            docs/code/test-coverage.md Cargo.toml Cargo.lock scripts/test-agents.sh)
WEB_PATHS=(apps/dashboard/src apps/dashboard/test apps/dashboard/package.json apps/dashboard/vitest.config.mts apps/desktop/src apps/desktop/test apps/desktop/package.json
           pnpm-lock.yaml scripts/test-agents.sh)
E2E_PATHS=("${RUST_PATHS[@]}" "${WEB_PATHS[@]}" crates/rolld/src apps/dashboard/public/install.sh apps/dashboard/next.config.ts scripts/agents-e2e.mjs)
TARGET=${CARGO_TARGET_DIR:-target}

force=0 unit_only=0
for a in "$@"; do
  case "$a" in
    --force) force=1 ;;
    --unit) unit_only=1 ;;
    *) echo "agents: unknown argument $a" >&2; exit 2 ;;
  esac
done

hash_of() { find "$@" -type f -not -path '*/target/*' -not -path '*/node_modules/*' 2>/dev/null | LC_ALL=C sort -u | xargs shasum -a 256 | shasum -a 256 | cut -d' ' -f1; }
# Whether a part must run: forced, or its files changed since the hash in its stamp.
stale() { [[ $force == 1 || ! -f $1 || "$(cat "$1")" != "$2" ]]; }
mkdir -p "$TARGET"

if ! git --version >/dev/null 2>&1; then
  echo "agents: the tests drive the real git client; install git" >&2
  exit 1
fi

# The Rust tests. Every cargo call names the same packages, so their features unify the same way
# and nothing builds twice; the test binaries then run side by side.
rust_tests() {
  local pkgs=(-p agent-proto -p agent-host -p model-gateway -p roll-cli -p code-control)
  local targets=(--lib --bin roll) f
  for f in crates/{agent-proto,agent-host,model-gateway}/tests/*.rs; do targets+=(--test "$(basename "$f" .rs)"); done
  # The relay's (its own, the agents rows of the auth matrix, the MCP agent tools, the coverage
  # map) and `roll agent`/`roll gateway` against a live relay.
  targets+=(--test agents --test agents_code --test agents_gateway --test automations --test linear --test coverage_map --test routes --test mcp --test agent --test gateway)
  cargo clippy "${pkgs[@]}" --all-targets -- -D warnings
  local list
  list=$(cargo test --no-run --message-format=json-render-diagnostics "${pkgs[@]}" "${targets[@]}" |
    jq -r 'select(.reason == "compiler-artifact" and .executable != null and .profile.test) | [(.manifest_path | split("/")[-2]), .target.name, .target.kind[0], .executable] | @tsv')
  local logs pids=() names=() pkg name kind exe args
  logs=$(mktemp -d "${TMPDIR:-/tmp}/roll-agents-tests.XXXXXX")
  while IFS=$'\t' read -r pkg name kind exe; do
    case "$pkg:$name:$kind" in
      code-control:code_control:lib) args=(agents cluster::) ;;
      code-control:routes:test) args=(agents) ;;
      code-control:mcp:test) args=(agent) ;;
      code-control:git:test) continue ;; # selected by agent-host's `--test git`; test:code runs it
      roll-cli:roll:bin) args=(coverage) ;;
      *) args=() ;;
    esac
    (cd "crates/$pkg" && "$exe" "${args[@]}") > "$logs/$pkg-$name.log" 2>&1 &
    pids+=($!) names+=("$pkg-$name")
  done <<< "$list"
  local i failed=0
  for i in "${!pids[@]}"; do
    if ! wait "${pids[$i]}"; then
      failed=1
      echo "agents: ${names[$i]} failed:" >&2
      tail -n 60 "$logs/${names[$i]}.log" >&2
    fi
  done
  echo "agents: ${#pids[@]} Rust test binaries: $(cat "$logs"/*.log | grep -E '^test result' | awk '{p += $4; f += $6} END {print p " passed, " f " failed"}')"
  rm -rf "$logs"
  return $failed
}

rust_hash=$(hash_of "${RUST_PATHS[@]}")
if stale "$TARGET/.test-agents-rust.hash" "$rust_hash"; then
  rust_tests
  echo "$rust_hash" > "$TARGET/.test-agents-rust.hash"
else
  echo "agents: Rust unchanged since its last green run; skipping it (--force to run anyway)"
fi

web_hash=$(hash_of "${WEB_PATHS[@]}")
if stale "$TARGET/.test-agents-web.hash" "$web_hash"; then
  pnpm --filter dashboard test
  pnpm --filter @roll/desktop test
  echo "$web_hash" > "$TARGET/.test-agents-web.hash"
else
  echo "agents: dashboard and desktop unchanged since their last green run; skipping them (--force to run anyway)"
fi

[[ $unit_only == 1 ]] && { echo "agents: unit tests green"; exit 0; }

e2e_hash=$(hash_of "${E2E_PATHS[@]}")
if ! stale "$TARGET/.test-agents.hash" "$e2e_hash"; then
  echo "agents: no changes since the last green end to end run; skipping it (--force to run anyway)"
  exit 0
fi
cargo build -p roll-cli -p rolld

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
s=$(date +%s)
node scripts/agents-e2e.mjs "$TARGET/debug"
echo "agents: end to end run took $(( $(date +%s) - s ))s"

echo "$e2e_hash" > "$TARGET/.test-agents.hash"
echo "agents: all green"
