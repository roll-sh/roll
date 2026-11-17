# roll apps deploy for GitHub Actions

Deploys a roll apps service from a workflow once its tests pass, waits until the deployment is live
or has failed, and says where it runs. On a pull request it can also keep one comment up to date
with that.

```yaml
on:
  push:
    branches: [main]
jobs:
  deploy:
    runs-on: ubuntu-latest
    needs: test
    steps:
      - uses: actions/checkout@v5
      - uses: roll-sh/roll/sdk/apps-github-action@main
        id: roll
        with:
          api-url: https://api.roll.example
          token: ${{ secrets.ROLL_TOKEN }}   # a roll token with deploy on the service
          service: svc_…
      - run: curl -fsS "${{ steps.roll.outputs.url }}/health"
```

What it deploys depends on the service's source:

- **A repo or Git URL service** builds `revision` (by default the workflow's commit) from the
  repository itself. Nothing leaves the runner.
- **An upload service** is sent the files committed under `path`, as `roll up` sends a tree. This
  is how a private repository deploys without the GitHub App. It uses `git archive`, so untracked
  files, ignored files and `.git` are never sent, and neither is anything an earlier step built.
- **An image service** deploys `image`, a tag or digest of its own repository (one the workflow
  pushed).

The inputs are:

- `api-url`, `token` and `service` (required).
- `environment` (default `production`) and `message` (default the commit's subject).
- `wait` (`false` only queues the deploy) and `timeout-minutes` (default 30).
- `show-logs`: when it's on and the deploy fails, the build's last lines are printed in the job.
  It's off by default, because a public repository's job log is public.
- `comment` and `github-token`: needs `permissions: pull-requests: write`.

The outputs are `deployment-id`, `status` and `url`. A retried step answers the deployment it
started, and a re-run attempt makes a new one.

GitHub gives a fork's pull request none of the repository's secrets, so such a run has no token
and deploys nothing. The action says that's why.
