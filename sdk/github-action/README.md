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

## Sign the job in to roll

For any product, not only secrets: the job signs in as a trusted machine client with its own OIDC
token, and later steps get `ROLL_TOKEN` (masked before it's written anywhere). Nothing is stored
in GitHub.

```yaml
permissions:
  id-token: write
steps:
  - uses: roll-sh/roll/sdk/github-action@main
    with:
      api-url: https://api.roll.example
      client-id: mcl_…   # roll token client create deploy --product apps --allow read,deploy --github owner/repo --environment production
  - run: roll apps deploy …   # the CLI reads ROLL_TOKEN
```

With an `environment` too, it signs in that way and then exports the environment's values as
above. `client-id` can't be combined with `identity` or `token`. A refusal names the client and
roll's reason ("the token's ref isn't the one this client trusts"), never the token.

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
