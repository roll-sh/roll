#!/usr/bin/env bash
# Keeps one GitHub issue open while production deploys fail (the deploy workflow runs it): a
# failed deploy opens it, or comments on it, with why; the next green deploy closes it.
#
#   report.sh failed <log>   the end of the deploy's log: what deploy.sh printed before rolling back
#   report.sh deployed       close the open issue, if any
#
# Needs GH_TOKEN (issues: write) and the workflow's GITHUB_* variables.
set -euo pipefail

LABEL=deploy-failure
TITLE="Production deploys are failing"
run="$GITHUB_SERVER_URL/$GITHUB_REPOSITORY/actions/runs/$GITHUB_RUN_ID"
commit=${TAG:-$GITHUB_SHA}

open_issue() { gh issue list --repo "$GITHUB_REPOSITORY" --label "$LABEL" --state open --json number --jq '.[0].number // empty'; }

case "${1:-}" in
  failed)
    log=${2:?the deploy log}
    # What deploy.sh says happened (alarms, breaching minutes, events, stopped nodes); a build
    # that failed before it got there leaves its last lines.
    why=$(sed -n '/deploy: deploy failed; what happened:/,$p' "$log" 2>/dev/null || true)
    [ -n "$why" ] || why=$(tail -n 80 "$log" 2>/dev/null || echo "no log")
    body=$(mktemp)
    {
      echo "The deploy of \`$commit\` failed and production still runs the previous build: $run"
      echo
      echo '```'
      # An issue body holds 65,536 characters.
      tail -c 60000 <<<"$why"
      echo '```'
      echo
      echo "\`deploy/cloud/deploy.sh status\` shows what's live; \`deploy/cloud/deploy.sh diagnose\` (or the deploy workflow run by hand with \`diagnose\`) reads more. The next green deploy closes this issue."
    } >"$body"
    gh label create "$LABEL" --repo "$GITHUB_REPOSITORY" --color B60205 --description "Production deploys are failing (opened by the deploy workflow)" >/dev/null 2>&1 || true
    n=$(open_issue)
    if [ -n "$n" ]; then
      gh issue comment "$n" --repo "$GITHUB_REPOSITORY" --body-file "$body"
    else
      gh issue create --repo "$GITHUB_REPOSITORY" --title "$TITLE" --label "$LABEL" --body-file "$body"
    fi
    ;;
  deployed)
    n=$(open_issue)
    [ -z "$n" ] || gh issue close "$n" --repo "$GITHUB_REPOSITORY" --comment "Deployed \`$commit\`: $run"
    ;;
  *)
    sed -n '2,8p' "$0" >&2
    exit 2
    ;;
esac
