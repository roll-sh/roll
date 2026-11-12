# Connecting GitHub: the roll GitHub App

roll code connects to GitHub through a GitHub App. roll lands changes itself and mirrors the
default branch back to GitHub, so the App needs to read the repository and push to it. What a
connected repo does is in [architecture.md](architecture.md#github-connected-repositories).

## 1. Create the App

Go to GitHub → Settings → Developer settings → GitHub Apps → **New GitHub App**, or do the same
under an organisation.

- **Homepage URL**: your roll web app, for example `https://roll.sh`.
- **Setup URL**: `https://<rolld>/v1/code/github/setup`. After installing, users land back on
  `/dashboard/code`. Tick *Redirect on update*.
- **Webhook URL**: `https://<rolld>/v1/code/github/webhook`. rolld has to be reachable from
  GitHub; for local development, use a tunnel such as `cloudflared` or `ngrok`.
- **Webhook secret**: a long random string. It becomes `ROLL_GITHUB_WEBHOOK_SECRET`.

Repository permissions:

| permission      | access         | why                                                           |
| --------------- | -------------- | ------------------------------------------------------------- |
| Contents        | Read and write | import history over git; mirror the default branch            |
| Pull requests   | Read and write | PRs become changes; comment on and close PRs that roll landed; resolve review threads (GraphQL) |
| Issues          | Read and write | issues become work items; close them when the work is done   |
| Checks          | Read and write | report roll's checks as check runs                            |
| Metadata        | Read           | required by GitHub                                            |
| Administration  | Read           | optional: read branch protection to show whether only roll can push |

Subscribe to these events: **Push**, **Pull request**, **Issues**, **Issue comment**. Installation
events are sent without subscribing.

roll also reads the App's own delivery log with its JWT (no extra permission): when rolld was
down, it asks GitHub to redeliver what failed (Settings → Advanced shows the same log), and
compares connected repos' branches and pull requests with its own state for anything older than
GitHub's three days.

Under *Where can this GitHub App be installed?*, pick *Any account* for a public service. For
a single organisation, pick *Only on this account*.

After creating the App, note its **App ID** and **slug** (the name in
`github.com/apps/<slug>`). Then generate a **private key**, which downloads a `.pem` file.

## 2. Configure rolld

```sh
export ROLL_GITHUB_APP_ID=123456
export ROLL_GITHUB_APP_SLUG=roll-code
export ROLL_GITHUB_PRIVATE_KEY_PATH=/etc/roll/github-app.pem   # or ROLL_GITHUB_PRIVATE_KEY with the PEM itself
export ROLL_GITHUB_WEBHOOK_SECRET=…
# GitHub Enterprise Server only:
# export ROLL_GITHUB_API_URL=https://ghe.example.com/api/v3
# export ROLL_GITHUB_WEB_URL=https://ghe.example.com
rolld
```

rolld logs `roll code: GitHub App configured`, and `GET /v1/code/github` returns
`{"configured": true, "app_slug": …, "install_url": …}`.

Also set `ROLL_WEB_URL` (where the web app runs) and `ROLL_CODE_PUBLIC_URL` (where rolld is
reachable). PR comments and the setup redirect link there.

## 3. Install and connect

1. In the dashboard, open **New repository → Connect GitHub**. If nothing is installed yet,
   follow the install link to GitHub and pick the repositories.
2. Pick a repository. roll imports its history, its open issues and its open PRs; progress shows
   on the repo page. Commit ids match GitHub's exactly.
3. On GitHub, protect the default branch and allow only the roll App to push:
   - Settings → Branches → the rule for `main`
   - *Restrict who can push*, with the App added
   roll is the landing authority. If people push to `main` directly, roll ingests the push, but
   the mirror pauses if both sides changed the same files.

Agents then work through roll: claim work items, land changes over the API or with
`git push origin HEAD:refs/land/main`. GitHub stays in sync and receives one push every few
seconds, however many changes land in between.

## 4. When either side is down

- **GitHub down**: roll keeps landing; the mirror catches up within about 30 s of GitHub coming
  back, and every comment, close, check run and review roll owes GitHub waits in roll's delivery
  queue until GitHub answers.
- **roll down**: GitHub's failed deliveries are redelivered and the repos reconciled when roll is
  back. Meanwhile `roll code land`, `roll code change create` and agent threads open pull requests
  on GitHub with the user's own GitHub login (`gh auth login`, `GH_TOKEN`, or git's credential
  helper); they land through roll once it's back. See
  [architecture.md](architecture.md#github-connected-repositories).
