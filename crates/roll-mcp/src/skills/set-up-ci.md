# Set up CI with roll

Use this when the user wants their GitHub Actions (or GitLab CI) to deploy, migrate or read
through roll. CI signs in with its own OIDC token as a roll *machine client*, so nothing is
stored in the CI system: no token in the repository's secrets.

## 1. The person makes the client (an agent's token can't)

Making a machine client is the operator's or a signed-in person's, never a token's, so ask the
user to run one command. Fill in the repository and what the job needs, nothing more:

```sh
roll token client create deploy-web \
  --product apps --allow read,deploy \
  --github OWNER/REPO --environment production
```

- `--ref refs/heads/main` instead of `--environment` limits it to one branch's jobs.
- Narrow the rules to what the workflow does: `--product postgres --allow read,write` for
  migrations, `--preset read-only` for a job that only reads.
- GitLab CI: `--gitlab group/project [--ref main]`; any other CI: `--issuer URL --subject PATTERN`.

The command prints the client id (`mcl_…`). It isn't a secret.

## 2. The workflow signs in

Add this to the job (the job needs `permissions: id-token: write`):

```yaml
permissions:
  id-token: write
  contents: read
steps:
  - uses: roll-sh/roll/sdk/github-action@main
    with:
      api-url: https://ROLL_API
      client-id: mcl_…
  - run: roll apps deploy …   # later steps have ROLL_TOKEN, masked
```

The token lives an hour and can do only what the client's rules allow. Revoking the client
(`roll token client revoke mcl_…`) stops it at once.

## 3. Check it

- After the first run, `roll token client list` shows the client's last sign-in.
- A refused sign-in names the claim that didn't match ("the token's ref isn't the one this client
  trusts"): compare it with the client's `--ref` or `--environment`.

Never put a roll token or a client secret in the workflow or the repository's secrets for this:
the point is that there's nothing to leak.
