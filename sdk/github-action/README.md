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

Test: `node --test test.js` (a fake rolld and a fake OIDC endpoint).
