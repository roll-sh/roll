# roll secrets for GitHub Actions

An environment of roll secrets in a job: logged in with the job's own OIDC token through a
workload identity (no stored token), every value masked, exported to the later steps.

```yaml
permissions:
  id-token: write
steps:
  - uses: roll-sh/roll/sdk/github-action@main
    with:
      api-url: https://api.roll.example
      identity: idn_…   # roll secrets identities add ci --github owner/repo --for-env staging
      environment: staging
  - run: make test
```

Inputs: `api-url`, `environment`, `identity` or `token`, and optionally `project`, `path` (a
folder), `audience` and `export` (`false`: to the step's outputs instead of the environment).
Output: `keys`, the names it read.

## Leak scanning

```yaml
on: pull_request
jobs:
  secrets:
    runs-on: ubuntu-latest
    permissions: { contents: read, id-token: write }
    steps:
      - uses: actions/checkout@v5
        with: { fetch-depth: 0 }
      - uses: roll-sh/roll/sdk/github-action@main
        with: { api-url: https://api.roll.example, identity: idn_…, scan: true }
```

The lines the pull request adds are checked against the project's stored values by fingerprint;
each one found fails the check with an annotation on its file and line, naming the key and the
environment, never the value. Output: `leaks`.

Test: `node --test test.js` (a fake rolld and a fake OIDC endpoint).
