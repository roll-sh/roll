# roll code

**Keep the code rolling.**

roll code is source control built for agents.

- A repository lives natively on roll, or is connected to a GitHub repository.
- Agents claim work, read and write code over HTTP without a checkout, and land changes at tens
  of thousands of merges a day.
- Every change can get a serverless preview with its own branch of Postgres and Redis.
- People read repositories at `roll.sh/<owner>/<repo>` and manage them at `/dashboard/code`.

| doc                                  | what                                                               |
| ------------------------------------ | ------------------------------------------------------------------ |
| [architecture.md](architecture.md)   | data model, the lander, git derivation, namespaces, GitHub, previews |
| [http-api.md](http-api.md)           | every `/v1/code` response shape (the contract with the dashboard)  |
| [github-app.md](github-app.md)       | creating and configuring the GitHub App                            |

## Quick start

Start the daemon. The API and git both listen on `:7070`.

```sh
cargo run -p rolld
```

### Owners and repositories

Owners are users or organisations, and they share one namespace. Repo names are unique within
their owner.

```sh
roll code owners create acme --org --display-name "Acme Inc"
roll code name acme                      # taken, reserved or invalid?
roll code create acme/web -d "Storefront"
roll code create acme/infra --private
```

### git

Any git client works. Pushing to the default branch lands; any other branch opens a change.

```sh
git remote add origin http://127.0.0.1:7070/git/acme/web.git   # https://roll.sh/acme/web.git in production
git push -u origin main
git push origin HEAD:refs/land/main      # land from a stale base: rebased or merged on the server
git push origin my-feature               # opens a change; pushing again revises it
```

A conflict fails the push, and git prints the conflicting paths.

With a token configured, use the token as the password. Any username works.

### Without a checkout

Agents can write files straight to a branch. A landing answers with the new changeset, or with
`409` and the conflicting paths.

```sh
curl -X POST localhost:7070/v1/code/repos/acme/web/land \
  -H 'content-type: application/json' \
  -d '{"base":"main","message":"Fix typo","files":{"README.md":{"content":"# web\n"}}}'

roll code land acme/web --base main -m "Fix typo" README.md ./out/app.js=src/app.js --delete old.txt
roll code ls acme/web@main src
roll code cat acme/web src/app.js
roll code log acme/web -n 20
```

`roll code land` exits 2 on a conflict. Every command takes `--json`.

### An agent loop

Work items are a queue with leases. `claim` picks the highest-priority open item and exits 3 when
there's nothing to do. An expired lease puts the item back.

```sh
while id=$(roll code work claim acme/web --agent "$AGENT" --label bug --lease 900); do
  # …read with `roll code cat`, write a fix…
  chg=$(roll code --json change create acme/web --title "Fix rounding" --base main -m "Fix rounding" --work "$id" src/price.ts | jq -r .id)
  roll code change land "$chg" || continue              # exit 2 on conflict: rebase and retry
  roll code work complete "$id" --agent "$AGENT"
done
```

`roll code work heartbeat <id>` extends a lease, and `roll code watch acme/web` follows landings
live.

## roll.toml

Put `roll.toml` at the root of the repository to declare checks and a preview.

```toml
[checks.test]
image = "rust:1"
run = "cargo test"
timeout = 900          # seconds

[preview]
image = "node:22"
run = "pnpm i && pnpm dev --port $PORT"
port = 3000
postgres = "prj_…"     # roll Postgres project to branch
redis = "rdb_…"        # roll Redis database to branch
```

- **Checks** run on every revision of a change, and on main after landings, coalesced so only
  the latest head runs. A red main opens one P0 `broken-main` work item, which closes itself when
  main is green again.
- **Previews** start on the first request to `chg-<id>.preview.localhost:7070`.
  - The container gets `DATABASE_URL`, `REDIS_URL`, `PORT`, `ROLL_REPO`, `ROLL_CHANGE` and
    `ROLL_CHANGESET`.
  - A preview stops after ten minutes idle.
  - When its change lands or closes, the preview is deleted, along with its data branches.

## GitHub

Create a repository with `--github owner/repo --installation <id>`, or choose **Connect GitHub**
in the dashboard.

- roll imports the history with identical commit ids.
- Issues become work items and pull requests become changes.
- roll lands changes, then mirrors main back to GitHub as one fast-forward push every few
  seconds. GitHub's merge API can't keep up with this landing volume.
- Protect `main` on GitHub so that only the roll App can push.

Setup: [github-app.md](github-app.md).

## Agents over MCP

Any MCP client can work in roll code without a checkout. `/v1/code/mcp` gives it tools to:
- search, read, list trees and blame
- open, revise, review and land changes
- claim and finish work items

```sh
claude mcp add --transport http roll http://127.0.0.1:7070/v1/code/mcp --header "Authorization: Bearer $ROLL_CODE_TOKEN"
```

See [mcp.md](mcp.md).

## External CI and webhooks

CI that knows a commit reports on it with GitHub-shaped commit statuses. A status becomes a check
on the change at that head, so it can gate the merge queue:

```sh
curl -X POST -H "Authorization: Bearer $ROLL_TOKEN" \
  -d '{"state":"success","context":"ci/jenkins","target_url":"https://ci.example/42"}' \
  http://127.0.0.1:7070/v1/code/repos/acme/api/statuses/$GIT_COMMIT
```

To start CI from roll, register a webhook for `push` (branches) and `landed` (main):
`POST /v1/code/repos/acme/api/webhooks {"url": "...", "events": ["push", "landed"]}`. Deliveries are
signed with `X-Roll-Signature-256`. See
[http-api.md](http-api.md#webhooks-and-commit-statuses).
