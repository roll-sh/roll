# Shared by the local gates (scripts/test-code.sh, scripts/test-agents.sh); sourced, not run.
#
# Every cargo call of both gates names the same packages, so features unify the same way and
# nothing builds twice: switching between the gates, or between a gate and its end to end binaries,
# reuses one set of artifacts. (Each `cargo test -p X` with its own package set re-resolved features
# and rebuilt: 19 min of test:agents and most of test:code's 31 min were such builds.)

TARGET=${CARGO_TARGET_DIR:-target}
PKGS=(-p agent-proto -p agent-host -p model-gateway
      -p code-sql -p code-cluster -p code-store -p code-git -p code-github -p code-index -p code-review -p code-chat -p code-control -p code-preview
      -p roll-cli -p rolld)
# rolld is linted by the other suites; linting it here would also lint every service it links.
CLIPPY_PKGS=("${PKGS[@]:0:${#PKGS[@]}-2}")

# The web part, the same in both gates and sharing one stamp: typecheck (TypeScript 7, ~3 s; the
# end to end build skips Next's own check because of it), the dashboard's vitest and the desktop's.
WEB_PATHS=(apps/dashboard/src apps/dashboard/test apps/dashboard/package.json apps/dashboard/vitest.config.mts apps/dashboard/tsconfig.json
           apps/desktop/src apps/desktop/test apps/desktop/package.json apps/desktop/tsconfig.json
           packages/brand/src packages/brand/package.json pnpm-lock.yaml scripts/typecheck.mjs scripts/lib/gate.sh)

gate_force=0
mkdir -p "$TARGET"

hash_of() { find "$@" -type f -not -path '*/target/*' -not -path '*/node_modules/*' 2>/dev/null | LC_ALL=C sort -u | xargs shasum -a 256 | shasum -a 256 | cut -d' ' -f1; }
# Whether a part must run: forced, or its files changed since the hash in its stamp.
stale() { [[ $gate_force == 1 || ! -f $1 || "$(cat "$1")" != "$2" ]]; }

need_git() {
  if ! git --version >/dev/null 2>&1; then
    echo "$1: the tests drive the real git client; install git" >&2
    exit 1
  fi
}

# web_tests <gate name>: the web part, when its files changed since its last green run.
web_tests() {
  local h
  h=$(hash_of "${WEB_PATHS[@]}")
  if stale "$TARGET/.test-web.hash" "$h"; then
    pnpm typecheck
    pnpm --filter dashboard test
    pnpm --filter @roll/desktop test
    echo "$h" > "$TARGET/.test-web.hash"
  else
    echo "$1: typecheck, dashboard and desktop tests unchanged since their last green run; skipping them (--force to run anyway)"
  fi
}

# rust_tests <gate name> <cargo target selection…>: builds the selected test targets of the package
# set in one cargo call, lints the package set (`RUST_CLIPPY=0` skips it), then runs the binaries
# the caller's `test_args pkg name kind` accepts, side by side: at most ROLL_TEST_JOBS at once (half
# the cores), the slowest of the last run first. `test_args` sets ARGS (the binary's arguments) and
# returns non-zero to skip a binary (a target of the same name in another package, say). Clippy
# runs first, not alongside: tests with real timeouts went red on a loaded machine when it did.
rust_tests() {
  local gate=$1
  shift
  local list
  list=$(cargo test --no-run --message-format=json-render-diagnostics "${PKGS[@]}" "$@" |
    jq -r 'select(.reason == "compiler-artifact" and .executable != null and .profile.test) | [(.manifest_path | split("/")[-2]), .target.name, .target.kind[0], .executable] | @tsv')
  local clippy=0
  if [[ ${RUST_CLIPPY:-1} == 1 ]]; then
    cargo clippy "${CLIPPY_PKGS[@]}" --all-targets -- -D warnings || clippy=1
  fi
  local times=$TARGET/.test-times-$gate logs half=$(( $(nproc) / 2 ))
  local jobs=${ROLL_TEST_JOBS:-$(( half > 4 ? half : 4 ))}
  logs=$(mktemp -d "${TMPDIR:-/tmp}/roll-$gate-tests.XXXXXX")
  touch "$times"
  local names=() pkg name kind exe
  while IFS=$'\t' read -r _ pkg name kind exe; do
    ARGS=()
    test_args "$pkg" "$name" "$kind" || continue
    while (( $(jobs -rp | wc -l) >= jobs )); do wait -n || true; done
    (s=$(date +%s%N) rc=0; cd "crates/$pkg" && "$exe" "${ARGS[@]}" || rc=$?
     echo "$rc $(( ($(date +%s%N) - s) / 1000000 ))" > "$logs/$pkg-$name.rc") > "$logs/$pkg-$name.log" 2>&1 &
    names+=("$pkg-$name")
  done < <(while IFS=$'\t' read -r pkg name kind exe; do
             # (Never an empty field: a tab is whitespace to `read`, so an empty one would vanish.)
             t=$(awk -v n="$pkg-$name" '$1 == n {print $2; exit}' "$times")
             [[ -n $exe ]] && printf '%s\t%s\t%s\t%s\t%s\n' "${t:-0}" "$pkg" "$name" "$kind" "$exe"
           done <<< "$list" | sort -t$'\t' -k1,1rn)
  wait
  if (( ${#names[@]} == 0 )); then
    echo "$gate: no test binaries matched; is the selection right?" >&2
    return 1
  fi
  local n failed=0 rc ms
  : > "$times"
  for n in "${names[@]}"; do
    read -r rc ms < "$logs/$n.rc" 2>/dev/null || { rc=1 ms=0; }
    echo "$n $ms" >> "$times"
    if [[ $rc != 0 ]]; then
      failed=1
      echo "$gate: $n failed:" >&2
      grep -E -A4 '^---- |panicked at' "$logs/$n.log" | head -n 60 >&2
      tail -n 15 "$logs/$n.log" >&2
    fi
  done
  echo "$gate: ${#names[@]} Rust test binaries: $(cat "$logs"/*.log | grep -E '^test result' | awk '{p += $4; f += $6} END {print p " passed, " f " failed"}'); slowest: $(
    sort -k2,2rn "$times" | head -4 | awk '{printf "%s %.1fs, ", $1, $2 / 1000}')"
  rm -rf "$logs"
  [[ $clippy == 0 ]] || echo "$gate: clippy failed" >&2
  return $(( failed | clippy ))
}
