# roll secrets

roll secrets stores the secrets and environment variables of a workspace project
(docs/workspace.md). A project has environments (`development`, `staging`, `production` and any the
team adds), each environment has folders, and each folder has secrets. Every write is versioned, an
environment can be rolled back to any point in time, values can reference other values, and a secret
can be **linked** to a roll resource in the project (a Postgres branch, a Redis database, a bucket,
a stream, a search cluster…) so it stays correct when that resource changes or rotates.

Infisical is the reference for depth (projects, environments, folders, references, imports,
versions, point-in-time recovery, personal overrides, rotation, dynamic secrets, approvals, audit,
`infisical run`). roll does better where it hosts the resources itself: links instead of pasted
connection strings, rotation and short-lived credentials issued by the product, and preview
environments that inherit from a base environment instead of copying it.

- Code: `crates/roll-secrets` (store, crypto, references, router), mounted by
  `crates/rolld/src/secrets.rs`, which also implements the link providers; the sync kinds are
  `crates/rolld/src/secrets/syncs.rs`.
- Client: `apps/dashboard/src/lib/secrets-api.ts`. Change it and this contract together.
- CLI: `roll secrets …` and `roll run` in `crates/roll-cli/src/secrets.rs`.
- Tests: `crates/roll-secrets/tests/api.rs` (the router in process, on SQLite or Postgres),
  `crates/roll-secrets/tests/nodes.rs` (two stores racing on one database),
  `crates/roll-cli/tests/secrets.rs` (`roll run`, with `--watch`, against roll secrets in process),
  `crates/rolld/tests/secrets.rs` (each product's link against the real rolld, and a sealed start),
  `apps/dashboard/src/lib/secrets-*.test.ts`. `pnpm test:secrets` runs them (`--docker`: on
  Postgres, and the Postgres link).

## Getting started

Everything below works the same from the dashboard (a project's **Secrets**) and the CLI.

1. **Install the CLI and sign it in** to your roll. `roll login` opens the browser (or shows a code
   to enter there, over SSH), and later commands go to that roll without `ROLL_API_URL` (with
   several logins, to the one you signed in to last; `ROLL_API_URL` always wins). roll has no hosted
   address yet, so give yours:

   ```
   curl -fsSL https://roll.sh/install.sh | sh
   roll login https://roll.example.com
   ```

   The consent screen starts from the `development` preset, where revealing a value asks you first:
   the first `roll run` prints where to approve it and waits.
2. **Pick a project and its environments.** A workspace project has `development`, `staging` and
   `production` the first time you open its secrets. Add your own (`roll secrets envs add qa`) and
   remember the project for this directory (a project that doesn't exist yet is offered to you to
   create; in a script, `--create` makes it):

   ```
   roll secrets setup -p shop -e development
   ```

   A repo can carry this for its teammates in `roll.toml` (never a URL, a token or a value):

   ```toml
   [secrets]
   project = "shop"
   environment = "development"
   branches = { "release/*" = "staging" }
   ```

   It's only a suggestion: a teammate runs `roll secrets setup` once, sees the table and approves it
   (`--approve` in a script), and a changed table asks again. It never chooses a protected
   environment, even approved: that takes `-e`, `ROLL_ENV` or your own setup. With no flags and no
   table, `setup` in a terminal lists the projects and environments you can see.
3. **Add values**, one by one or from a `.env` file (one change, which you can roll back):

   ```
   roll secrets set STRIPE_KEY=sk_test_… LOG_LEVEL=debug
   roll secrets import .env
   roll secrets set DATABASE_URL='postgres://app:${PASSWORD}@${staging.DB_HOST}/shop'   # references
   ```
4. **Link roll resources** instead of pasting their credentials. A linked value follows the
   resource: a rotated password, a new key. In the dashboard, **Link a resource**: a Postgres
   branch's URL, a Redis URL, a bucket's own access key, a stream's API key, a search API key.
5. **Run with them.** Nothing is written to disk; `--watch` restarts on a change:

   ```
   roll run -- npm run dev
   roll run -e production --watch -- ./server
   ```
6. **Promote** what changed from one environment to the next, and **protect** production so writes
   become change requests someone else approves:

   ```
   roll secrets diff --from development --to staging
   roll secrets promote --from staging --to production STRIPE_KEY
   roll secrets envs protect production --approvals 1
   ```
7. **CI and services** get a roll token (roll-access's, docs/permissions.md) with one rule on the
   project: some environments, some folders and only the actions they need, with an expiry. By
   default a token reads and reveals one environment for 90 days: the one `--for-env` names, else
   the directory's own unless it's protected (a protected one only by name). Everywhere
   (`--all-environments`) and never expiring (`--no-expiry`, with a warning) are asked for:

   ```
   roll secrets tokens create ci --for-env staging
   ROLL_TOKEN=rlt_… roll run -e staging -- make test
   ```

   Or push an environment out with a **sync** (GitHub Actions secrets, a sealed webhook, a deploy
   notification). roll's own checks (`secrets = "development"` in `roll.toml`), previews, apps and
   sandboxes read their environment directly.
8. **From code**, a service reads its environment with `@roll/secrets` (`packages/secrets`) and the
   same token. `resolve` with `sealTo` returns the values sealed to a key only that process holds,
   and `watch` follows the digest and renews dynamic credentials' leases, as `roll run --watch` does:

   ```ts
   import { createClient, generateKeyPair, loginWithIdentity } from "@roll/secrets";

   const secrets = createClient({ api: process.env.ROLL_API_URL!, token: process.env.ROLL_TOKEN! });
   const { values } = await secrets.values("staging");
   const keys = generateKeyPair();
   const env = secrets.open(await secrets.resolve("production", { consumer: "app:web", sealTo: keys.publicKey }), keys.secretKey);
   const stop = secrets.watch("staging", { onChange: (v) => reconfigure(v.values) });

   // A process that reads values per request keeps them a while: within the TTL from memory,
   // then a conditional read that costs a 304 (nothing read, issued or audited) while unchanged.
   const cached = createClient({ api: process.env.ROLL_API_URL!, token: process.env.ROLL_TOKEN!, cacheTtlMs: 30_000 });

   // A reader that can't renew a lease (a playbook, a one-off script): no dynamic credential is
   // issued, those keys are left out and named in `dynamic`, and the answer can be kept.
   const { values: plain, dynamic } = await secrets.values("staging", { dynamic: false });

   // One key's writes (`PUT`/`DELETE …/secrets/{key}`). A set replaces a link the key had; a
   // delete of a key that isn't there is `{deleted: false}`. A protected environment, or a folder
   // that needs approval, throws `SecretsError` with the code `approval_required` (open a change
   // request instead). Errors never hold the value, and both drop the client's cached reads.
   await secrets.set("staging", "FEATURE_FLAGS", "beta", { path: "/api", comment: "rollout" });
   const { deleted } = await secrets.delete("staging", "OLD_KEY", { path: "/api" });

   // One key's reads, for a tool that writes only what differs (a playbook's check mode). `secret`
   // is the key's record without its value (`GET …/secrets/{key}`: `version`, `comment`, the
   // link's `link_kind` only, and `source`: own, base or import), null when the folder has no such
   // key. `storedValue` is the value as stored, references unresolved (`…/value?resolve=false`):
   // a reveal, audited and never cached, and no error holds it.
   const record = await secrets.secret("staging", "FEATURE_FLAGS", { path: "/api" });
   const stored = await secrets.storedValue("staging", "FEATURE_FLAGS", { path: "/api" });

   // A workload identity's login with the workload's own OIDC token: `tokenSource` is called for a
   // fresh one at each login (the first, then before the roll token ends and when it's refused).
   const ci = await loginWithIdentity({ api: process.env.ROLL_API_URL!, identity: "idn_…", tokenSource: () => readFile(tokenFile, "utf8") });

   // An encryption key (below, "Encryption keys"): the key stays in roll.
   const cards = secrets.transit("cards");
   const ct = await cards.encrypt(cardNumber, `order:${id}`);
   const index = await cards.hmac(email); // a lookup column over encrypted emails
   ```

   The Go and Python clients name these `Values(…, secrets.WithoutDynamic())`, `Set`, `Delete`,
   `Secret` (a `*SecretInfo`, nil when missing), `StoredValue` and
   `secrets.LoginWithIdentity(ctx, api, identity, tokenSource)`, and `values(…, dynamic=False)`,
   `set`, `delete`, `secret` (a `SecretInfo`, None when missing), `stored_value` and
   `roll_secrets.login_with_identity(api, identity, token_source)`. All three export the OIDC
   token source their `fromEnv`/`FromEnv`/`from_env` uses (`oidcTokenFromEnv`, `OIDCTokenFromEnv`,
   `oidc_token_from_env`): `ROLL_OIDC_TOKEN`, the file `ROLL_OIDC_TOKEN_FILE` names, or GitHub
   Actions' token for `ROLL_OIDC_AUDIENCE` (the API's URL by default). In TypeScript,
   `await fromEnv()` reads `ROLL_API_URL`, `ROLL_TOKEN` or `ROLL_IDENTITY`, and `ROLL_PROJECT`.

   In Go, `sdk/go/secrets` has the same surface, and `FromEnv` logs a workload identity in with
   its own OIDC token as `roll run` does (`ROLL_IDENTITY`). It logs in again before the login's
   token ends, and when the server refuses it, reading the OIDC token afresh each time, so a
   long-running watch keeps working:

   ```go
   c, err := secrets.FromEnv(ctx)
   err = c.Watch(ctx, "production", secrets.Watch{OnChange: func(v *secrets.Values) error { return reload(v.Values) }})
   ```

   In Python, `sdk/python/roll_secrets` (`roll-secrets`, the standard library but for sealed
   resolves, which need the `seal` extra):

   ```python
   c = roll_secrets.from_env()
   c.watch("production", lambda v: reload(v.values))
   ```

   In Java, `sdk/java/secrets` (`sh.roll:roll-secrets`, Java 17 and the JDK only):

   ```java
   RollSecrets c = RollSecrets.builder().build(); // ROLL_API_URL, ROLL_TOKEN or ROLL_IDENTITY, ROLL_PROJECT
   Watch w = c.watch("production", WatchOptions.of(v -> reload(v.values())));
   ```

   A Spring Boot app imports a folder into its `Environment` with `sdk/java/secrets-spring`
   (`sh.roll:roll-secrets-spring`, Spring Boot 3): `spring.config.import=roll-secrets:production/api`
   in `application.properties` (`optional:roll-secrets:…` starts without it when roll can't answer
   or refuses), the `roll.secrets.*` settings or the variables above, dynamic keys left out unless
   `roll.secrets.include-dynamic`, and with `roll.secrets.refresh=true` the watch swaps new values
   in and publishes a `RollSecretsRefreshedEvent`.

   In .NET, `sdk/dotnet/secrets` (`Roll.Secrets`, .NET 8 and the base class library only, X25519
   its own), with `AddRollSecrets` for `IConfiguration` in `Roll.Secrets.Configuration`:

   ```csharp
   using var c = new RollSecretsClient(); // ROLL_API_URL, ROLL_TOKEN or ROLL_IDENTITY, ROLL_PROJECT
   await using var w = c.Watch("production", new WatchOptions { OnChange = v => Reload(v.Values) });
   builder.Configuration.AddRollSecrets("production", s => s.ReloadOnChange = true);
   var ct = await c.Transit("cards").EncryptAsync(cardNumber, $"order:{id}");
   ```

   In Ruby, `sdk/ruby/roll_secrets` (`roll_secrets`, the standard library only, sealed resolves
   included; its README loads values into a Rails app at boot):

   ```ruby
   c = RollSecrets.from_env
   c.watch("production") { |v| reload(v.values) }
   ```
9. **When something goes wrong**: `roll secrets history`, `roll secrets rollback --at 2h`, the
   audit log (`roll secrets audit`, or streamed to your SIEM), and `roll secrets share KEY` to hand
   one value to someone outside roll through a link that works once.

## Terraform

`sdk/terraform-provider-roll` manages roll secrets as code, as Doppler's and Infisical's providers
do. It speaks the same HTTP API as the CLI and the clients, with a roll token or the operator's.

- **`roll_secrets_environment`** ensures an environment (`PUT …/environments/{env}`) and sets its name
  and whether it's protected. The policy is only written when `protected` changes, so approvers and
  a notify URL set in the dashboard stay. Destroying one passes its confirmation.
- **`roll_secret`** is one key in a folder: a value, or a `link` to a resource on the canvas. On
  refresh it reads the stored value back (`resolve=false`, references as written; the token needs
  `reveal`), so a change made outside Terraform shows in the plan. A key that only shows through a
  base or an import isn't the resource's. In a protected environment a write opens a change
  request instead, which Terraform reports as an error.
- **`roll_secrets_identity`** is a workload identity (CI's OIDC login) and **`roll_secrets_sync`** a
  sync to a platform. roll changes neither in place, so every attribute replaces them. roll makes
  identities for people and the operator only, never for a token. A sync's target is listed by host
  only, so a target changed outside Terraform isn't seen; its signing secret (webhooks) is shown
  once and kept, sensitive, in the state.
- **`roll_secrets_token`** is a token for CI or a service: a roll token with one rule on the
  project's secrets, made through `POST /v1/access/tokens` as the dashboard's New token. Its
  secret is in the state (sensitive). Any change replaces it, and destroying it revokes it.
- **`roll_machine_client`** is roll-access's: a machine client that signs in with CI's own OIDC
  token or a public key, never a secret (docs/permissions.md, "Machine clients"). It needs the operator's token.
- **`data.roll_secrets_values`** is an environment's resolved values, for another provider.
- **What a project registers itself**: **`roll_secrets_ca`** (domains, `allow_ip`, `max_days`; the
  CA's `certificate` out), **`roll_secrets_ssh_ca`** (principals, host domains; its `public_key` and
  `known_hosts` out), **`roll_secrets_transit_key`** (an encryption key, with `deletion_protection` on by
  default: what it encrypted can't be read once it's destroyed, so a rename or removal fails until
  it's turned off) and **`roll_secrets_outside_database`**
  (roles, group, CA; its admin URL `admin_url_wo` is write-only, Terraform 1.11+: sent once and
  sealed in roll, never in the state, and bumping `admin_url_wo_version` registers it again). roll
  changes none in place, so every attribute replaces them; keys link to them as to any resource.
- **More of what a project registers**, each imported by `prj_…/<id>` (or the id alone, in the
  provider's project). Credentials going in are write-only (Terraform 1.11+), with a `…_wo_version`
  to send a new one:
  - **`roll_secrets_rotator`**: its name and URL change in place (`PATCH`). A URL that carries a
    token goes in `url_wo` (bump `url_wo_version` to send a new one, in place), never in `url`,
    which is in the plan and the state. Its signing secret is shown once, so it's kept, sensitive,
    in `secret` from creation. Changing `rotate_secret` (a keepers map) rotates it
    (`…/rotate-secret`) and keeps the new one.
  - **`roll_secrets_audit_sink`**: an HTTPS endpoint, or `bucket:`, `search:` or `stream:` on the
    canvas. A SIEM `format` takes `token_wo`. A roll sink's signing secret is kept in `secret`.
  - **`roll_secrets_github_app`**: `private_key_wo`, never returned.
  - **`roll_secrets_aws_role`** and **`roll_secrets_aws_user`**: both change in place (`PATCH`),
    except a user's IAM user name.
  - **`roll_secrets_cloud_account`**: `cloud = "gcp"` or `"azure"`, with that cloud's fields.

  Sinks, Apps and accounts have no `PATCH`, so every attribute replaces them.
- **Project settings**, imported by `prj_…`:
  - **`roll_secrets_required_keys`** owns the project's whole required list (`PUT …/required`), so
    keep one per project; destroying it requires nothing.
  - **`roll_secrets_kms_key`** is the customer-managed key (`PUT`/`DELETE …/kms`): a new ARN changes
    in place, and destroying it unwraps the data key.
- **Without values in the state** (Terraform 1.10 and 1.11+), as Vault's and AWS' providers allow:
  - **`ephemeral "roll_secrets_values"`** reads an environment for one run, never into the plan or
    the state; it feeds another provider's configuration or a write-only argument. Strict: a key
    that doesn't resolve fails the run. With `dynamic = true` a dynamic link issues a credential for
    the run, and closing the run revokes its lease.
  - **`value_wo`** on `roll_secret` writes a value write-only: it's sent and never kept, and
    `value_wo_version` is bumped to write a new one (Terraform can't see a write-only value
    change). Such a value isn't read back, so a change made outside Terraform isn't seen.

`value` and the data source still land in Terraform's state, as with every provider: keep that
state as private as the values, or use the two above.

## Pulumi

Pulumi runs the same provider through its [`terraform-provider`](https://www.pulumi.com/registry/packages/terraform-provider/)
package, which bridges any Terraform provider from a local build, with no registry release
(`sdk/pulumi`). In a Pulumi project:

```sh
(cd path/to/roll/sdk/terraform-provider-roll && go build -o "$OLDPWD/bin/terraform-provider-roll" .)
pulumi package add terraform-provider ./bin/terraform-provider-roll   # sdks/roll: @pulumi/roll, pulumi_roll, Go
pulumi config set roll:apiUrl https://api.roll.example
pulumi config set roll:project prj_…
pulumi config set --secret roll:token rlt_…
```

```ts
import * as pulumi from "@pulumi/pulumi";
import * as roll from "@pulumi/roll";

const staging = new roll.SecretsEnvironment("staging", { slug: "staging", name: "staging" });
new roll.Secret("stripe", {
  environment: staging.slug,
  key: "STRIPE_KEY",
  valueWo: new pulumi.Config().requireSecret("stripeKey"),
  valueWoVersion: 1,
});
const ci = new roll.SecretsToken("ci", { name: "ci", environments: [staging.slug], actions: ["read", "reveal"] });
export const token = ci.token; // a Pulumi secret
```

Every resource and both data sources (`getSecretsValues`, `getAppService`) are there. Each machine
that runs the program needs the binary at the path in `Pulumi.yaml`, and `pulumi install` generates
the SDK from it. `sdk/pulumi/sdk.sh` generates the SDKs outside a project. Where Pulumi differs:

- **No values out of the state.** Pulumi has no ephemeral resources, so there's no
  `roll_secrets_values` for one run and no dynamic credential revoked when the run ends. Pulumi also
  keeps every input in the stack's state, so `valueWo` is there as `value` is. Both are encrypted by
  the stack's secrets provider, and `valueWo` is never read back. To keep values out of a state
  entirely, read them where they're used (`roll run`, the clients) instead of through Pulumi.
- **Secrets.** Anything the provider marks sensitive is a Pulumi secret, encrypted in the state: the
  provider's token, `value`, `valueWo`, a token's `token`, a sync's `signingSecret`, an app variable's
  value, a service's `gitToken` and a notification target's `url` and `secret`. A function's results
  aren't: wrap `getSecretsValuesOutput(…).values` in `pulumi.secret()` before it reaches a stack
  output or another resource.
- **Names.** Pulumi auto-names an optional `name` (an environment's becomes `staging-1a2b3c4`): set
  it.

`sdk/pulumi/e2e.sh` runs `pulumi up` and `pulumi destroy` against the provider's fake rolld.
## GitHub Actions
`sdk/github-action` puts an environment into a job, as Doppler's and Infisical's actions do:
```yaml
permissions:
  id-token: write
steps:
  - uses: roll-sh/roll/sdk/github-action@main
    with:
      api-url: https://api.roll.example
      identity: idn_…          # a workload identity for this repo (roll secrets identities add ci --github owner/repo --for-env staging)
      environment: staging
  - run: make test            # DATABASE_URL and the rest are in the environment
```
- The job logs in with its own OIDC token (audience: the API's URL), so no token sits in the
  repository's settings; `token` takes a roll token instead.
- Every line of every value, the OIDC token and the roll token are masked (`::add-mask::`) before
  anything else is printed. Values are exported to `GITHUB_ENV` (or, with `export: false`, the
  step's outputs) inside a random heredoc delimiter no value contains, so a multi-line value
  (a PEM key) arrives whole and can't add variables of its own. A key that doesn't resolve fails
  the step, naming it.
- No dependencies: Node's own `fetch`, nothing to bundle or audit.
- **`client-id: mcl_…`** signs the job in to roll for any product as a trusted machine client
  (docs/permissions.md, "Machine clients": `roll token client create --github owner/repo`), with
  the same OIDC token, and exports that token, masked first, as `ROLL_TOKEN`; with an
  `environment` it then reads the values with it. It can't be combined with `identity` or `token`.
- **`scan: true`** guards a repository on GitHub the way roll code guards the ones it hosts: the
  lines a pull request adds (`git diff base...HEAD`; check out with `fetch-depth: 0`) are cut into
  candidates as `roll secrets scan` cuts them and checked with `POST …/leaks` (fingerprints; only
  values the token may reveal), and each stored value found fails the check with an annotation on
  its file and line naming the key and environment, never the value.
## Ansible
`sdk/ansible` is the `roll.secrets` collection, as Vault's `community.hashi_vault` and 1Password's
`onepassword.connect` are, built on the Python client (`sdk/python/roll_secrets`) with the same
settings (`ROLL_API_URL`, `ROLL_TOKEN` or `ROLL_IDENTITY` with an OIDC token, `ROLL_PROJECT`):
```yaml
- ansible.builtin.template: { src: api.env.j2, dest: /etc/api.env, mode: "0600" }
  vars:
    db_url: "{{ lookup('roll.secrets.secret', 'DB_URL', environment='production', path='/api', project='shop') }}"
  no_log: true
- roll.secrets.secret: { environment: staging, key: LOG_LEVEL, value: info }   # or state: absent
  delegate_to: localhost
```
- The lookup returns values, or a folder with `as_dict=true`, marked unsafe so Ansible never
  templates them. It reads with `dynamic=false`: a playbook renews no lease, so dynamic keys are
  left out and none is issued unless `dynamic=true` asks. Reads go through the client's cache
  (`cache_ttl`, 30 s), which spans one task's worker process.
- The module sets or deletes one key and is idempotent (the same value writes nothing). Check mode
  compares the stored value on the host running the module (`reveal`), and the result never holds
  the value (`value` is `no_log`). Errors name the key and roll's message, never a value.
## Azure Pipelines

A pipeline logs in with its own OIDC token, as GitHub Actions' jobs do: nothing long-lived in its
variables. Azure DevOps signs a pipeline's token as its organization
(`iss` `https://vstoken.dev.azure.com/<organization id>`, the GUID in `$(System.CollectionId)`),
for Azure's audience (`aud` `api://AzureADTokenExchange`; a pipeline can't ask for another), with
the pipeline as its subject (`sub` `p://<organization>/<project>/<pipeline>`):

```sh
roll secrets identities add deploy --for-env production --audience api://AzureADTokenExchange \
  --issuer https://vstoken.dev.azure.com/<organization id> --subject p://acme/shop/deploy
```

```yaml
variables:
  ROLL_API_URL: https://roll.example.com
  ROLL_IDENTITY: idn_…
steps:
  - script: roll run --env production -- ./deploy.sh
    env:
      SYSTEM_ACCESSTOKEN: $(System.AccessToken)   # a secret: a script sees it only when mapped
```

With `ROLL_IDENTITY` set and no `ROLL_TOKEN`, roll asks Azure for the token itself
(`POST $(System.OidcRequestUri)?api-version=7.1` with the job's access token; the answer's
`oidcToken`) and logs in. Elsewhere, put the token in `ROLL_OIDC_TOKEN` yourself. The dashboard's
New identity has an Azure Pipelines choice that fills in the issuer and audience.

- The audience is Azure's, not roll's, so a token the pipeline got for Azure itself would also
  log in here: that's why the subject names one pipeline exactly. A pipeline's subject is its
  name, and two pipelines in different folders of one project can share one, so whoever may
  create a pipeline in that project could make another `deploy`. For production, use a service
  connection instead, whose use its own pipeline permissions, approvals and checks guard: an Azure
  Resource Manager connection with workload identity federation, `ROLL_AZURE_SERVICE_CONNECTION`
  set to its id, and the identity's issuer and subject copied from the connection's "Workload
  Identity federation details" (older connections: `sc://<organization>/<project>/<connection>`
  from the organization's issuer; newer ones: Entra's issuer and an `/eid1/…/sc/<organization id>/<connection id>` subject).
- Roll treats Azure's issuers like the shared ones: an identity whose subject could match more
  than one pipeline or connection is refused.

## Kubernetes
Three ways, as the best products offer:
- **Mounted (`sdk/csi-provider`)**: a provider for the [Secrets Store CSI
  Driver](https://secrets-store-csi-driver.sigs.k8s.io), as Vault's, AWS', Azure's, GCP's and
  Infisical's: a pod's volume gets an environment's values as files when it starts, and no
  Kubernetes Secret is made (nothing in etcd) unless the SecretProviderClass asks the driver to
  sync one. Each pod logs in with its own service account token (the driver's `tokenRequests`,
  audience the API's URL) through a workload identity, so a pod reads only what its service
  account's identity allows; the cluster's keys can be pinned (#57) where roll can't reach its
  issuer. Without an identity, a roll token from the volume's `nodePublishSecretRef` (`token`).
  The read is strict: a value that doesn't resolve fails the mount (the pod waits and the event
  names the key), and on the driver's rotation the files keep what they had. Dynamic credentials
  only with `dynamic: "true"` (nothing renews their leases: each ends with its TTL). The folder's
  digest is each file's version.
  ```yaml
  apiVersion: secrets-store.csi.x-k8s.io/v1
  kind: SecretProviderClass
  metadata: { name: shop-production }
  spec:
    provider: roll
    parameters:
      api: https://api.roll.example
      identity: idn_…            # the pod's service account logs in (roll secrets identities add)
      environment: production
      path: /                    # the folder
      format: dotenv             # files (one per key: the default), dotenv or json
      file: .env                 # dotenv's and json's file name
      keys: "DATABASE_URL=db-url STRIPE_KEY"   # optional: which keys, renamed with `=`
  ```
  Install the driver with `tokenRequests[0].audience=<the API's URL>`, then the provider
  (`sdk/csi-provider/chart`, or `deploy/provider.yaml`): a DaemonSet that writes `roll.sock` into
  the driver's provider directory. It needs no Kubernetes permissions. A pod's token goes only to
  an `https://` API (`-allow-http` for a cluster-local rolld), and `-api` (the chart's `apis`) pins
  which APIs SecretProviderClasses may name; a mount, login and read together, takes 30 s at most.
- **Pull (`sdk/kubernetes-operator`)**: a `RollSecret` names an environment and a folder (and a
  project or an identity, when not the operator's own), and the operator keeps a Secret beside it
  in step. It logs in with its own service account token through a workload identity, so roll
  holds no credential for the cluster. It long-polls each RollSecret's digest
  (`…/digest?since=&wait=`), so a change reaches the Secret within seconds, and checks every 30 s
  anyway; it reads values only when the digest moved; it writes only Secrets it made (labelled, owned by their RollSecret, so they go with
  it) and leaves a same-named Secret someone else made alone; values that don't resolve leave the
  Secret as it was. Its status says what happened. Outside the operator's own namespaces a
  RollSecret logs in as a service account of its own namespace (`spec.serviceAccountName`, a
  10-minute token the operator requests), so creating a RollSecret never reaches what the
  operator's identity, or another team's, reaches. `spec.rolloutRestart` names the Deployments,
  StatefulSets and DaemonSets that read the Secret: a change restarts them (`kubectl rollout
  restart`'s annotation), each one that opts in with a `roll.sh/restart-on` annotation naming the
  RollSecret (so a RollSecret's author can't restart a workload that didn't ask). One that couldn't
  be restarted waits in `status.pendingRestarts` and is tried again on every pass.
- **Push (the `kubernetes` sync)**: roll writes a Secret with credentials for the cluster it holds,
  sealed. For clusters roll can reach and the operator can't run in.

Access *to* a cluster is the other way round: roll issues short-lived service account tokens per
lease (["Kubernetes service-account tokens"](#kubernetes-service-account-tokens)).
## AWS Lambda
`sdk/lambda-extension` is a layer, as AWS's Parameters and Secrets Lambda Extension: it answers a
function's values on `127.0.0.1:2773` (`GET /secrets?env=&path=` as JSON, `GET /secrets/{KEY}` as
text, each with `X-Roll-Token: $AWS_SESSION_TOKEN`), loads `ROLL_PREFETCH` folders before the first
invocation, and keeps them `ROLL_CACHE_TTL` (300 s), then revalidates with a 304; a folder with a
dynamic credential is kept only within its lease. It logs in as a workload identity with a JWT STS
signs for the function's own role (`GetWebIdentityToken`, IAM's outbound identity federation: the
account's own issuer, `sub` the role's ARN), so neither the function nor its configuration holds a
roll token; `ROLL_TOKEN` otherwise. Not yet run against a real account (its README, "Gaps").
## Customer-managed keys
A project's data key can be wrapped once more by a KMS key in the customer's own AWS account, as
Infisical's external KMS does (`PUT …/kms {key: "arn:aws:kms:…"}`, `roll secrets kms set ARN`;
admin). Opening it then needs both roll's deployment key and the customer's key, so the customer
can make the project's values unreadable to roll at once by revoking roll's grant or disabling the
key. The values themselves aren't re-encrypted; only the data key is wrapped.
- **The key policy** grants roll's role `kms:Encrypt` and `kms:Decrypt` with the conditions
  `kms:EncryptionContext:roll = secrets` and `kms:EncryptionContext:project = prj_…`. Setting a key
  encrypts and decrypts with it first, and also tries a made-up project: a key that would wrap any
  project's data key (another customer's, which roll's role may be allowed to use) is refused, so
  no tenant can bind their project to someone else's key.
- **Format**: `kms:v1:<key>:<deployment key id>:<ciphertext>`, wrapping the data key as roll's
  deployment key seals it; the KMS encryption context is `{roll: secrets, project: <id>}`, in the
  key's own region.
- **Revocation** takes effect within a minute: an unwrapped key (still sealed by the deployment
  key) is kept in memory for 60 s, so a request doesn't wait on KMS. After that the project answers
  423 `kms_denied`, naming the key and KMS's reason, never a value (and `roll run --fallback`
  doesn't start from its file on it); a KMS that doesn't answer is 503. A refusal is remembered as
  long, so a revoked project doesn't ask KMS at every request. Changing the wrapping (`PUT` or
  `DELETE …/kms`) always asks KMS, never the memory: a revoked key can't be swapped out.
- **Rotating the deployment key**: the id of the deployment key inside tells health which wrapped
  projects are still sealed by the previous one, and the sweeper reseals them (50 a pass), so the
  previous key is dropped only once health says `all`. A reseal KMS won't wrap leaves the project
  readable and is tried again. `DELETE …/kms` unwraps it (KMS must still answer). Every change is
  in the audit log (`project.kms`).
## AWS
Short-lived AWS credentials for each lease, as Vault's AWS engine and Infisical's dynamic secrets
give: a project registers a role in its own account (`POST …/aws-roles {name, role_arn, trust?}`,
`roll secrets aws-roles add`, the Credentials tab; admin), and three keys link to it in `dynamic`
mode (`access_key_id`, `secret_access_key`, `session_token`: one credential, so they share one
session per resolve). Each resolve assumes the role for the lease's TTL plus five minutes (15
minutes to 12 hours, within the role's own maximum). The role trusts roll one of two ways:
- **roll's issuer** (`trust: "oidc"`, the dashboard's default), as GCP and Azure work: an IAM OIDC
  identity provider for roll's issuer (`/v1/secrets/oidc`, audience `sts.amazonaws.com`), and a
  trust policy with the conditions `aud = sts.amazonaws.com` and `sub = roll-secrets:<project>`.
  A role for an app's instances instead (`roll-apps:…`, "roll's tokens for other products") pins
  the exact `sub`, environment included (`StringEquals`, never `StringLike` with `*`). Each session is `AssumeRoleWithWebIdentity` with a token roll's issuer signs for the project:
  roll holds no AWS credential at all. The issuer must be public over HTTPS
  (`ROLL_CODE_PUBLIC_URL`).
- **ExternalId** (`trust: "external_id"`, the API's default): roll's own AWS role assumes it (STS
  AssumeRole with roll's credentials) with the condition `sts:ExternalId = roll-secrets:<project>`.
- **Either way** registering a role assumes it once, and first tries a made-up project (its
  ExternalId, or a token for it): a role that takes any (another customer's that trusts roll, say)
  is refused, so no tenant can register and assume someone else's role.
- **Ending a lease** doesn't end its session early (STS can't revoke one session): it stays valid at
  AWS until it expires, the TTL plus five minutes at most. To cut every session at once, revoke the
  role's older sessions in IAM. A session can't be renewed: resolve again for a new one.
- A role keys still link to can't be deleted (409 `in_use`); deleting the project deletes its roles.

**IAM users' access keys**, rotated by roll as Infisical's AWS IAM rotation does, for what can't
use a role's sessions (a vendor that takes only a key pair, an old CI). A project registers an IAM
user next to one of its roles (`POST …/aws-users {name, role, user}`, `roll secrets aws-users add
NAME USER --role ROLE`, the Credentials tab; admin): roll acts as that role (one that trusts roll
by the project's ExternalId), whose policy allows `iam:ListAccessKeys`, `iam:CreateAccessKey` and
`iam:DeleteAccessKey` on the user, and registering lists the user's keys to check. It's the link kind
`aws_iam_user`, `managed` (`access_key_id` and `secret_access_key`, one credential `key`).

- **Two key slots take turns.** Each issue (linking, every `rotate_days`, `…/rotate`) creates a key,
  and the key it replaces is deleted when its grace period ends.
- **Nothing is pruned to make room.** A user that already holds two keys is refused (409 `in_use`,
  naming both): one may be in use (a key of the customer's, the previous one still in its grace,
  another environment's). Rotating twice within one grace period is refused the same way.
- A user keys still link to, or with a retired key still to delete, can't be deleted, nor a role
  that users act as.

## Where it runs

roll secrets is always on, like workspace projects: it is not a product in `ROLL_PRODUCTS`.

- On a single node, the data is SQLite at `<home>/secrets.db`.
- With the system database (`ROLL_SYSTEM_DATABASE`), it is Postgres, in the schema `secrets`. Nodes
  share it without leases: every read-then-write is one `code_sql` write transaction, and write
  closures only touch the database (audit rows are written in the same transaction; anything
  outside the database, such as revoking a credential, happens after `write` returns).
- Background work (expiring ephemeral environments and leases, due rotations) runs on every node,
  and each item is claimed in a write before it is acted on, so two nodes never act on one item.

## Encryption

Values are encrypted at rest with envelope encryption:

```
ROLL_SECRET_KEY ──HKDF-SHA256(salt "roll-secrets", info "kek:v1")──▶ KEK (AES-256-GCM)
KEK seals each project's random 32-byte data key  (aad "roll-secrets:dek:<project>")
data key encrypts every value                     (aad "roll-secrets:v1:<project>:<secret id>:<version>")
```

- **The deployment key** is `ROLL_SECRET_KEY` (at least 32 characters), the key that already seals
  review keys, chat tokens and sandboxes. Without it, a single node generates a key file at
  `<home>/secrets.key` (mode 0600) and logs that it did. A node using the system database refuses to
  serve secrets without `ROLL_SECRET_KEY` (every route answers 503 and says why), because nodes must
  share the key.
- **The deployment key from AWS KMS.** Instead of the key itself, a node can hold it encrypted:
  `ROLL_SECRET_KEY_KMS` is the base64 `CiphertextBlob` of
  `aws kms encrypt --key-id <key> --plaintext fileb://key --encryption-context roll=secrets`.
  rolld decrypts it once at startup with the standard AWS credentials (an instance or task role on
  roll cloud), so the plaintext key is never in the environment or on disk, a stolen environment
  is useless without KMS access, and every startup is a CloudTrail event. The encryption context
  binds the ciphertext to roll secrets. Rotation works the same with `ROLL_SECRET_KEY_KMS_PREVIOUS`.
  If KMS can't decrypt it, every route answers 503 with KMS's error code (never key material), and
  health reports `"key": "kms"`.
- **Split into shares.** `roll secrets key split` and `combine` hold the deployment key as N-of-M
  shares, and rolld can start sealed and be unsealed by them, with the key never in its
  environment or on its disk (see "Splitting the deployment key" and "Starting sealed").
- **Key rotation.** Set the new key as `ROLL_SECRET_KEY` and the old one as
  `ROLL_SECRET_KEY_PREVIOUS`. A data key that opens only with the previous key is resealed with the
  new key on first use, so the previous key can be dropped once `GET /v1/secrets/health` reports
  `"resealed": all`.
- **Values are bound to their row.** The associated data names the project, the secret's id and its
  version. A ciphertext copied to another row, project or version doesn't decrypt.
- **Fingerprints.** Each version stores `fp`, the first 8 bytes of HMAC-SHA256(fingerprint key,
  value), where the fingerprint key is derived from the project's data key. The overview grid uses
  fingerprints to flag environments whose values differ, without revealing any value. They reveal
  equality inside one project only.
- **Never in the clear outside a reveal.** Values never appear in logs, `Debug` output, errors,
  audit rows or change summaries. The `Value` type redacts itself in `Debug` and `Display` and is
  zeroized on drop. Error messages name keys, never values.
- **Pending values** (change requests waiting for approval) are encrypted the same way, with
  `roll-secrets:request:<request id>:<n>` as the associated data.

## Splitting the deployment key

As Vault's Shamir unseal and recovery keys: the deployment key (or a backup's secret key) is split
into shares held by different people, and any threshold of them give it back, so no one person
holds it and losing one holder doesn't lose it. Fewer than the threshold reveal nothing about the
key. It's a ceremony on an operator's machine: `roll secrets key` never talks to roll, and no
share goes to rolld, the database or a log.

```
roll secrets key split --shares 5 --threshold 3 --file key        # or the key on stdin
roll secrets key combine --out key                                 # shares on stdin, or asked for
```

- **The scheme.** Shamir's secret sharing over GF(2^8) (`crates/roll-keyshares`, with the FIPS-197
  field vectors in its tests). A share is `rlks_` and base64url of the split's random id, the
  threshold, the share's index, its bytes and a checksum. A mistyped or cut-short share, a share of
  another split (an older one, or another key's) and a share given twice are each refused by name
  (`share 2 is from a different split than share 1`). What's shared carries a check of the key, so
  shares that don't belong together give an error, never a wrong key.
- **Splitting.** `split` reads the key from `--file` (`<home>/secrets.key` on one node), else stdin,
  asked for without echo on a terminal; from KMS, `aws kms decrypt … --query Plaintext --output text
  | base64 -d | roll secrets key split …`. Surrounding whitespace isn't part of the key, as for rolld.
  It prints one share per line, or writes `roll-key-share-N-of-M` files (0600) with `--out-dir`.
- **Who holds them.** One share per person, each kept apart from the others and from the servers:
  a password manager vault of their own, or a sealed envelope in a safe. Threshold 3 of 5 tolerates
  two holders being away or gone. Record who holds which index, never a share, in the runbook.
- **Recovering.** Any threshold of the holders run `combine` on one machine, each typing or pasting
  their share when asked (nothing echoes; a bad share is refused and asked for again). The key goes
  to stdout or `--out` (0600), from where it's set as `ROLL_SECRET_KEY` (or encrypted for
  `ROLL_SECRET_KEY_KMS`) as usual: `ROLL_SECRET_KEY="$(roll secrets key combine)" rolld …`. Or
  rolld starts sealed and holders give it their shares themselves (next section).
- **Rotating.** Shares follow the key: after a rotation (see "Encryption"), split the new key and
  have holders destroy the old shares once health reports `"resealed": all` and the previous key is
  dropped. A new split has a new id, so an old share mixed into it is refused, not misread.
- **Backups without the one holder.** A backup opens only with its secret key (see "Backup and
  restore"); split that key the same way, so any threshold of holders restore it:
  `roll secrets keygen --out k && roll secrets key split --shares 5 --threshold 3 --file k --out-dir
  shares && rm k`, then at restore time `roll secrets key combine --out k` and `restore … --key k`.

### Starting sealed

As Vault's Shamir unseal: with `ROLL_SECRET_KEY_SEALED` instead of `ROLL_SECRET_KEY`, rolld starts
without the deployment key, and holders give it their shares over the API until it has a
threshold of them. The key is then only in rolld's memory: never in its environment, a file or a
log.

```
ROLL_SECRET_KEY_SEALED=3f9c0a1b2c3d4e5f:3 rolld …   # as `roll secrets key split` prints it
roll secrets unseal --status                          # sealed, 0 of 3 shares
roll secrets unseal                                   # each holder: their share, without echo
```

- **What runs while sealed.** Nothing but the unseal: no product starts, nothing is opened with the
  key, and every other route (health included, so load balancers keep the node out) answers 503
  `{"sealed": true}`. `ROLL_SECRET_KEY_SEALED` is `<split id>:<threshold>`: naming the split
  means only its shares count, whoever posts first. It can't be set beside `ROLL_SECRET_KEY`,
  `ROLL_SECRET_KEY_KMS` or their `_PREVIOUS`: finish a rotation (health reports `"resealed": all`)
  before starting sealed, and split the new key.
- **Unsealing.** `POST /v1/secrets/unseal {"share": "rlks_…"}` takes one share and answers the
  progress, `{"sealed": true, "have": 1, "need": 3}`; `GET` answers the same without a share. No
  answer or log carries a share. `roll secrets unseal` reads the share without echo on a terminal
  (else a line on stdin) and posts it; `--status` only asks. A mistyped or damaged share is
  refused, and the others stay. A share of another split drops every share given so far, and so
  does a threshold whose shares don't combine or whose key isn't the store's: the holders start
  over, and the log says why (never with a share).
- **The key is checked against the store.** roll secrets records the deployment key's id (the
  12-hex-character id customer-wrapped keys already carry, which reveals nothing of the key) each
  time it opens with a key that opens its data, and a rotation moves it on. The combined key must
  be the one recorded, or open a project's data key. A store with nothing to check against yet (a
  new one) takes the first key whose split `ROLL_SECRET_KEY_SEALED` names.
- **Who may unseal.** The route needs no token (it's public in rolld's gate): before the key exists
  there's nothing to authenticate against, so the shares are the credential, as in Vault. One
  address may post 10 shares a minute; a split may fail 5 times an hour (failures are kept in the
  store, so a restart doesn't clear them, and every node of a fleet counts them together), and
  then answers 429 with `Retry-After`. Shares given wait 10 minutes at most, in memory only, and
  are zeroized when dropped. A sealed node's API is still only reachable where its address is
  (the edge's public listeners don't start until it's unsealed): give holders that address, not a
  public one.
- **Then.** Every product starts as it does with `ROLL_SECRET_KEY`, given the key in memory: roll
  secrets (health reports `"key": "shares"`), and the products that seal with it, roll apps'
  variables, roll search's cluster secrets, the edge's certificates, the audit log's sinks, roll
  code's reviewer keys, chat tokens and sandboxes, and accounts' authenticator seeds. The API keeps
  its port throughout: connections made while the products start wait for them.
- **A fleet.** Each node unseals on its own, as Vault's do: `roll secrets unseal --node
  http://10.0.1.5:7070 --node http://10.0.1.6:7070 …` gives one holder's share to every node at
  once. Unsealed nodes don't pass the key to sealed ones: whatever authenticated that hand-off (the
  cluster secret, in every node's environment) would then be worth the key itself.
- **A restart starts sealed again.** Nothing of the key or the shares outlives the process, so
  every restart (a deploy, a crash, a rolling upgrade, one node at a time) needs a threshold of
  holders again. Where that's too much for routine restarts, KMS (`ROLL_SECRET_KEY_KMS`) is the
  unattended choice, with the shares kept for recovery.

## Model

```
project (a workspace project, prj_…)
└─ environment (env_…)  slug, name, position, kind: standard | ephemeral
   ├─ base: another environment (ephemeral environments only)
   ├─ policy: protected, approvals, approvers
   └─ folder "/", "/api", "/api/stripe"
      ├─ imports: [{environment, path}]
      └─ secret (sec_…)  key, value, comment, tags, type (shared | personal), link
         └─ version 1, 2, 3 … (each made by a change)
```

### Environments

A project starts with `development`, `staging` and `production`, in that order, the first time
anything touches its secrets. Teams add, rename, reorder and delete their own environments.

- **Slug**: 1–32 characters of `[a-z0-9-]`, not starting or ending with `-`, unique in the project.
  References, the CLI and the URLs use the slug. **Renaming** an environment's slug rewrites every
  reference to it in the project (`${staging.X}` becomes `${qa.X}`) in the same transaction, as one
  change per environment that had a reference, so nothing breaks.
- **Name**: what the dashboard shows, 1–48 characters.
- **Position**: the order of the dashboard's columns and of promotion (dev → staging → prod).
- **Delete** removes the environment's folders, secrets, versions, requests and leases (leases are
  revoked first). A protected environment can only be deleted with `?confirm=<slug>`. The last
  standard environment can't be deleted.

### Ephemeral environments (previews)

roll previews (docs/previews.md) makes one environment per change or pull request. An **ephemeral**
environment is a first-class kind, built to be cheap:

- It has a **base** (a standard environment, e.g. `staging`). It stores only its overrides: a key it
  doesn't set is read from the base, at the same path, at read time. Creating one copies nothing, so
  creating or deleting hundreds is a few rows each.
- It carries **labels** (`{"change": "chg_42", "pr": "117"}`, keys and values of `[a-z0-9_.-]`, up
  to 16) and an optional **expiry**. It is deleted when it expires (its leases are revoked first),
  and `DELETE …/environments?label=pr=117` deletes every environment with that label in one call.
- One call creates the environment together with its first secrets and links. roll previews
  writes its branched resources' addresses (the preview's Postgres branch, its Redis branch) as
  plain values: a branch is the preview's own resource, not a link to a parent on the canvas.
- **Links to what the preview branched aren't followed.** An ephemeral environment carries
  **branches**, `[{kind, parent}]`: the resources its preview branched (`{"kind": "postgres",
  "parent": "pg_staging"}`). A key it reads, from its base or anywhere else (a reference, an import),
  whose link names `(kind, ref)` with `ref` a branch's `parent` never resolves there: roll secrets
  doesn't ask the product, issues nothing, takes no lease and decrypts no managed credential, in
  any mode (`live`, `managed`, `dynamic`, a role's fields too). The key is left out of the values
  and named in the answer's `followed`, `[{name, kind, parent, field}]` (`field`: the provider
  field it's linked to, `url`, `password`, `role_url`…), and roll previews sets each from its
  branch by field, blanking one its branch has no value for. So a preview's code, a fork's
  included, never gets staging's database password through the base. A value of the
  environment's own that references a followed key doesn't resolve either (a problem). This holds
  for every read that returns values or tells of them (`…/values` and its formats, `…/resolve`
  sealed or not, a key's reveal, a sync, roll apps' resolve, `…/digest` and the watches, caches,
  External Secrets Operator, the operator and the CSI provider that read through these): a
  followed key's part of the digest is fixed, so its parent rotating moves nothing and isn't
  asked. Ensure sets the list with `branches`: given, it replaces the list; absent, it keeps it. A
  standard environment refuses it (400). Only roll previews changes it (the operator, as its
  adapter calls the API, or its in-process consumer `roll:previews`; 403 `forbidden` otherwise):
  an added entry changes what previews must fill, and a removed one would hand the preview's code
  the parent's values. An ensure that gives the same list, or none, needs nothing more. A backup keeps the list, and a
  restore that can't set it doesn't restore the environment.
- It is hidden from the overview grid's columns (the grid shows a "Previews" count instead), is
  never a promotion target, and can't be protected. Its slug is any free slug (`pr-117`).
- **Ensuring one** is a single idempotent call, `PUT …/environments/{env}` with what it should be
  (`{base?, kind?, name?, labels?, expires_at? | ttl_minutes?, trusted?, path?, secrets?: Op[],
  replace?, branches?}`). If the environment is missing, it's created (201). Otherwise its labels,
  expiry, name and branches are brought to what's given, and `secrets` are applied to its own keys at `path` as one
  change, with `replace` also deleting the folder's other own keys (never a linked or personal
  one). An unchanged value is no change, so ensuring what's there records nothing (`change: null`).
  Its kind, base and trust aren't changed in place: 409 `mismatch`, delete it and ensure it again.
  previews' reconcile loop and apps' environments are each one call.
- It is **untrusted** unless made with `"trusted": true` (which needs `admin` on its base): it
  runs a change's code, so reading it never reaches a protected environment (its base, an import, a
  reference), whoever reads it. A preview of a same-repo change in a project that allows it is made
  trusted; a fork's never is. `Environment.trusted` says which.

#### For roll previews: branches and `followed`

Each converge's ensure names what the preview branched, from the base's links (`ref` is the
parent's id as the link names it):

```json
PUT /v1/secrets/projects/prj_1/environments/pv-42
{
  "base": "staging",
  "labels": { "preview": "pv-42" },
  "ttl_minutes": 1440,
  "trusted": false,
  "secrets": [{ "op": "set", "key": "DATABASE_URL", "value": "postgres://…pv-42…" }],
  "replace": true,
  "branches": [
    { "kind": "postgres", "parent": "pg_staging" },
    { "kind": "bucket", "parent": "assets" }
  ]
}
```

A resolve then leaves every key linked to those parents out of `values` (none of them was read or
issued) and names it:

```json
POST /v1/secrets/projects/prj_1/environments/pv-42/resolve
{ "path": "/", "consumer": "preview:pv-42/web", "untrusted": true, "dynamic": false }

{
  "values": { "API_URL": "https://staging.example.com", "DATABASE_URL": "postgres://…pv-42…" },
  "problems": [],
  "leases": [],
  "dynamic": [],
  "linked": [],
  "withheld": [],
  "sensitive": [],
  "followed": [
    { "name": "PGHOST", "kind": "postgres", "parent": "pg_staging", "field": "host" },
    { "name": "PGPASSWORD", "kind": "postgres", "parent": "pg_staging", "field": "password" },
    { "name": "S3_SECRET", "kind": "bucket", "parent": "assets", "field": "secret_access_key" }
  ]
}
```

Previews sets each `name` from its branch's `field` (blank where the branch has none). A sealed
resolve carries the same `followed` beside the box, in the clear. An own key (`DATABASE_URL`
above) wins over the base's as always, so a key previews already writes isn't followed.

### Folders

A path is `/` or `/` followed by 1–8 segments of 1–64 characters of `[A-Za-z0-9_-]`, joined by `/`.
Folders exist explicitly (they can be empty). Writing a secret creates the folders on its path.
Deleting a folder deletes what is under it, as one change.

### Secrets

- **Key**: `[A-Za-z_][A-Za-z0-9_]*`, at most 256 characters, unique per environment, path and
  owner. Keys are case-sensitive, like environment variables.
- **Value**: UTF-8, at most 64 KiB (multi-line values such as PEM keys are fine).
- **Comment**: at most 1,000 characters. **Tags**: at most 16, each 1–32 characters of
  `[a-z0-9-]`.
- **Type**: `shared` (everyone with access reads it) or `personal`. A personal value is an override
  that belongs to one principal: when that principal reads the environment, its personal value
  replaces the shared one. No other caller ever sees a personal value, and tokens can't own one.
- **Link** (optional): the value comes from a roll resource. See Links.

### Changes and versions

Every write is a **change**: an atomic batch of operations on one environment, with a message, an
author and a time. Every change gets the next number in its project (`seq`). Each operation makes a
new version of its secret, and a delete makes a tombstone version, so history is complete.

- **Check-and-set**, as Vault KV's `cas`: a set or delete may carry `if_version` (`PUT
  …/secrets/{key} {…, if_version}`, `DELETE …/secrets/{key}?if_version=`, each op of a batch), the
  version the writer read; `0` means the key must not exist yet. It's checked in the same
  transaction as the change: if the key moved, the whole change is refused (409 `stale`, naming
  the key and its version now) and nothing is written. The dashboard's editor sends the version it
  showed, and so does setting a key across environments from the grid (each environment's own
  entry, or 0 where it had none), so two people editing one key can't overwrite each other unseen
  (the second sees the error and the new version). On a protected environment a stale read is
  told so before anything else, and a change request keeps the check: `if_version` is checked as
  the request is opened, in the write that records what it's based on, and applying it later needs
  the environment unchanged since. `if_seq` on a batch is the same for the whole environment
  (`roll secrets edit` uses it).
- **History** of an environment is its changes, newest first. A change's detail lists each key with
  what happened (`added`, `changed`, `deleted`, `comment`, `tags`, `link`), its old and new
  fingerprints, and never a value. Values of old versions can be revealed one at a time.
- **Point-in-time rollback.** `POST …/rollback {"to": seq}` or `{"at": ms}` computes the
  environment's state at that point from its versions and writes one new change that brings the
  environment back to it: it sets keys that changed, restores keys that were deleted and deletes
  keys that didn't exist yet. Rollback never rewrites history: the rollback itself can be rolled
  back.
- **Snapshot.** `GET …/changes/{seq}` with `?state=1` lists every key as it was after that change
  (metadata and fingerprints).
- **Values as they were.** `GET …/values?seq=` (or `?at=` a time) answers a folder's own values as
  they were stored after that change, without rolling anything back: "what did production have
  before the incident". They're as written then: a reference isn't resolved against the past, and
  a live or dynamic link, which stores nothing, and a purged value come back as problems. It needs
  `reveal`, is audited as an export ("as of change #N"), and takes `format=` like any export.
  `roll secrets export --change N` (or `--at 2h`), and the history's "Values then" in the
  dashboard.

### Purge and retention

After a leak and a rotation, the old value shouldn't stay readable in history.
`POST …/secrets/{key}/purge?path= {before_version?}` (`admin`) drops the stored ciphertext of the
key's older versions (default: all but the current one), which nothing can decrypt again. The
history rows stay, marked `purged`, so history still says what changed and when. Revealing a purged
version answers 404. A rollback that would need a purged value leaves that key as it is and says so
in its `warnings`. A project can also keep old values for a while only
(`PATCH /v1/secrets/projects/{project} {history_days}`, 1–3650, `null` for good): the sweeper purges
versions older than that, never a secret's current version.

### References

A value can include other values:

| Syntax | Means |
| --- | --- |
| `${KEY}` | `KEY` in the same environment and folder |
| `${env.KEY}` | `KEY` at the root of environment `env` |
| `${env.folder.sub.KEY}` | `KEY` in folder `/folder/sub` of environment `env` |
| `$${` | a literal `${` |

- References are resolved on the server when values are read with `resolve` (the default for
  `values`, `export` and `roll run`). A reference to another environment needs `reveal` access to
  that environment too.
- Resolution follows references up to 8 deep, and a cycle is reported with its path:
  `cycle: DB_URL → DATABASE_URL → DB_URL`. A missing key or an unreadable environment leaves the
  reference unresolved. Reads report it in `problems`, and `roll run` fails on it unless
  `--allow-unresolved` is passed.
- A write that adds a reference to a key that doesn't exist, or that makes a cycle, succeeds and
  returns the problem in `warnings`. The dashboard shows it before saving.

### Imports

A folder can import other folders (`[{environment, path}]`, in order): their secrets show up in the
folder as if they were its own, read-only. For each key, the folder's own secret wins, then the
imports in the order listed, and then the base environment (for an ephemeral environment). Imports
are followed up to 8 deep, and cycles are ignored and reported. Imported keys are marked with where
they come from.

#### Sharing across projects

An organisation-wide value (an error tracker's DSN, a shared API key) lives in one project and is
shared with the others, with both sides' consent:

1. The owning project's admin **exports** a folder to a project: `PUT …/environments/{env}/exports?path=`
   `[{project}]` (admin on that folder; audited `exports.set`). `roll secrets exports add <project>`.
2. That project's admin **imports** it like any folder, naming the project:
   `{"project": "<id>", "environment": "production", "path": "/shared"}` (admin on the importing
   folder, since every reader of it will read the shared values; refused with 403 unless the folder
   is shared with this project). `GET …/shared` lists what other projects share with this one,
   ready to import. `roll secrets imports add production:/shared --from-project <id>`.

Readers of the importing folder then see the shared values as part of their project, a token
scoped to it too: the export stands in for access to the other project, the one exception to
"nothing reaches an environment its caller can't read", made explicit by the other project's admin.
Values stay encrypted with their own project's data key and are opened with it. The export is
checked on every read, so removing it cuts every import of it off at once. Changes to a shared
value move the importing folder's digest (`roll run --watch` and syncs follow it).

What crosses is stored values only: plain values and managed credentials, never personal values,
the other folder's own imports, a value with a reference, or a live or dynamic link (those would
resolve against the other project's environments and resources).

In the dashboard the imports panel has "From another project" and, under it, "Shared with other
projects" for the folder's own exports; an imported key names the project it comes from.

A protected environment's shared folder never reaches an untrusted consumer (a check, sandbox or
preview), as its own never does. Promotion copies only the folder's own keys (a shared one is the
target's to import, or not). A shared value's fingerprint is its own project's, so compare and the
grid show it apart from a copy of the same value here, and its cell has no fingerprint here.

### Required keys

A project can list keys every standard environment must define, each in a folder
(`PUT …/required [{key, path?, comment?}]`). The overview grid shows a required key as a row even
before any environment sets it (`required: true`). Reading an environment's values reports each
required key it lacks as a problem (`required, but staging doesn't set it`), so `roll run`, checks
and resolves refuse to start without it, as they do for an unresolved reference. An ephemeral
environment counts what its base defines.

**Typed keys.** A required key can also say what its value must be: a `type` (`url`, `int`,
`port`, `bool`, `json`, `email`, `totp`: see TOTP keys) and a `pattern` (a regular expression the whole value matches;
at most 500 characters, checked when it's set). A write, an import, a promotion, a change request
or a new ephemeral environment's overrides with a value that doesn't fit is refused with 400
naming the key and the rule (`PORT must be a port (1–65535)`), never the value. A value with a
reference is checked once resolved: reading the environment's values reports `PORT must be a port`
as a problem, so `roll run` won't start with it. A rollback restores what was there, typed or not.
Edit them in Environments → Required keys, or `roll secrets required add PORT --type port`.
A type added after values exist doesn't change them: the overview grid marks each stored value
that doesn't fit (`misfit` on its cell, the rule it breaks), for a caller who may reveal it.
The dashboard's add and edit forms check a typed key's value as it's typed, with the same rules
and messages (`apps/dashboard/src/lib/secrets-required.ts`).

**Password policies** (Vault's password policies, AWS Secrets Manager's generator rules). A
required key can say what its values are made of: `password: {length, classes?, symbols?}`, a
length of 8–256 every value has at least, and the classes (`lower`, `upper`, `digit`, `symbol`;
default the first three) a value must use one of each of and nothing outside them. `symbols` says
which characters `symbol` means (1–32 ASCII punctuation; default `!#%&*+-=?^_~`), for a target's own
rules: no `@` or `:` in a connection URL's password, no quotes for a shell. Writes are checked as
typed keys are (`DB_PASSWORD needs at least a digit`, never the value). The generators follow it:
`roll secrets set KEY --generate` makes `length` characters of its classes, one of each (a longer
`--generate N` if asked), and so do the dashboard's Generate and Health's New value buttons. Set one in
Environments → Required keys (a length and a set of classes) or with `roll secrets
required add DB_PASSWORD --password-length 32 --classes lower,upper,digit,symbol --symbols '-_'`.

### TOTP keys

A key whose value is a TOTP seed (RFC 6238) hands out the current one-time code, as Vault's TOTP
engine and 1Password's one-time passwords do: CI that logs into a 2FA-protected registry or vendor
console reads the code, never a person's phone. A key is one when it's typed `totp` (a required
key, `roll secrets required add REGISTRY_TOTP --type totp`) or its value is an `otpauth://totp/`
URI.

- **The value** is an `otpauth://totp/…?secret=…` URI (`algorithm` SHA1, SHA256 or SHA512;
  `digits` 6–8; `period` 15–120 s; other parameters, such as `issuer`, are kept and ignored) or a
  bare base32 seed (any case, spaces allowed), which means SHA1, 6 digits and 30 s. The secret is
  80–1024 bits. A typed key's write with a seed that doesn't parse is refused with 400 naming the
  rule (`REGISTRY_TOTP must be a TOTP seed (…): digits must be 6–8`), never the value. Otherwise
  it's a secret like any other: versions, audit, sensitive, references, protected environments.
- **`GET …/secrets/{key}/totp`** answers `{code, expires_at, period}` (`expires_at` in ms: the end
  of the code's step). It needs `reveal`, as a reveal does (the code is a credential), reads the
  seed through the same path (a sensitive key's code reaches only roll's runtimes, an untrusted
  caller gets one only where it could reveal the seed, a reference resolves), and is audited as
  `secrets.totp` with the key's name, never the code: a read of the key, as a reveal is (insights,
  health's `unread`, reach, an alert for a new address). A key that isn't a TOTP key is a 400.
- **Reads with codes.** `…/values?codes=1` and a resolve with `"codes": true` add `<KEY>_CODE`
  beside each TOTP key that parses (its seed stays under `<KEY>`; a name the folder already uses is
  left alone; a sensitive key's code is sensitive too). `roll run --totp-codes` asks for them. A
  code is valid until its step ends, at most `period` seconds: it suits a command that logs in
  right away, not a long-running one (`--watch` doesn't restart for a new code). Such a read is
  never a 304. `roll secrets totp KEY [-e ENV] [--path P] [--wait]` prints the code and the seconds
  left; `--wait` waits for the next code when fewer than 5 s are left.
- The dashboard shows a TOTP key's code on its row (**Code**) with a ring counting down its step,
  hidden again as a revealed value is (typed keys only: the row doesn't know an untyped key's
  value). The clients ask for a code (`totp(env, key)` in TypeScript, `TOTP` in Go, `totp` in
  Python) or compute one from a seed they already resolved, a sealed resolve's say (`totpAt`,
  `TOTPAt`, `totp_at`), each checked against RFC 6238's vectors.

### Owners

A required key can name its `owner` (a plain name: letters, digits, spaces and `@._-/`, since it goes
into chat notices; `roll secrets required add STRIPE_KEY --owner @payments`, the
Environments view's Owner field): the person or team who supplies it. Health's findings about that
key carry it (`owner`, "ask @payments" in the view), and so does the daily notice, so whoever sees
an expired or missing key knows who decides its new value. The key's rule in its own folder
counts first, else its rule anywhere.

### Insights

`GET …/environments/{env}/insights?path=` shows, for each key of a folder, when it last changed
and by whom, when its value was last read and by whom, how often in the last 30 days, and whether
it's **stale**: unchanged for longer than the environment's `max_age_days`
(`PATCH …/environments/{env} {max_age_days}`, 1–3650, `null` for never). Reads come from the audit
log (reveal, export, resolve and sync pushes) over the last 90 days; nothing is decrypted. A stale
key is only flagged: it doesn't stop `roll run`, a deploy or a check.

**Daily notices.** An environment with a notify URL (its protection policy's `notify_url`) gets one
notice a day with its health findings (see Health): expired, missing, exposed, expiring and stale
keys, worst first, each with its detail (`findings`), up to 10 per kind in the text, and a link to
the Health view. Weak values aren't in it (that says something about a value, and a channel's
readers needn't be ones who may reveal it), nor unread ones. Stale needs a `max_age_days`; live
and dynamic links never count as stale (they change on their own), managed ones do. The same
outbox and retries as approval notices carry it; two nodes never both send it, since the day is
claimed in the same write that queues the notice, and an environment with nothing to say is
looked at once a day too.

**Expiry dates.** Some values stop working on a date roll can't see: a vendor's API key, a TLS
certificate, a trial token. A secret's optional `expires_at` (ms; set in any write's `set`
operation, `null` clears it, leaving it out keeps it) records that date. It's versioned like the
comment, so history, rollback, promote, change requests and backups carry it, and a new value
keeps the date until someone changes it, except a certificate: a new value holding PEM
certificates is dated by the earliest `notAfter` among them, unless the write gives a date. Insights add `expires_at` and `expiring` (within 14 days,
or past); the grid and the environment's list show a clock, with the days left inside two weeks
and "expired" in red after. The daily notice above lists expiring and expired keys too (names and
dates only), for any environment with a notify URL, limit or not. Like
staleness, it's only flagged: nothing stops working in roll. `roll secrets set KEY=v --expires
2027-03-31`, `roll secrets expire KEY --on 90d` (or `never`) and the editor's date field set it.

**Protected values set elsewhere** (1Password Watchtower's reuse check). In a protected
environment, each key's `exposed_in` names the unprotected environments that hold the very same
value: production's database password pasted into staging, where more people, previews and less
vetted code can read it. Values are compared by fingerprint (per project; nothing is decrypted),
plain values only: links and references are expected to match, and the empty value is skipped.
Only environments the caller may read are named. The environment's list marks the key "also in
staging", and `roll secrets insights` prints `ALSO IN staging`.

**Read alerts** (Doppler's and 1Password's activity alerts). The environment's notify URL also
hears of reads that look unlike the reader, from the audit log, within a sweep of happening:
- a token or workload identity reading from an address it hasn't read from before (its first
  address isn't news: that's where it runs);
- one principal revealing 30 or more of the environment's values within an hour (someone copying
  it out by hand).

An alert names the principal, the address or the count, the environment and a link to its
activity: never a value. Each is also an audit row (`secrets.alert`, by `roll:alerts`), told or
not: the environment's Activity has an Alerts filter, and audit sinks carry them to a SIEM. The sweep walks the audit log after a cursor; advancing the cursor,
recording the addresses seen and queueing the notices are one write, so two nodes never both
alert, and history from before the first sweep isn't alerted on.

**Weak values** (1Password Watchtower). A key whose name says it's a credential (`PASSWORD`,
`SECRET`, `TOKEN`, `API_KEY`, `PRIVATE_KEY`…, but not `…_URL`, `…_HOST`, `…_ID` and the like) is
judged when it's written, from the plaintext then: a common password, its own key's name, under
12 characters, too few different characters, or under 64 bits of randomness (as if each character
were drawn from its classes). Only the reason is kept (`weak`, never anything of the value), on
`Secret` and in insights, and only a caller who may reveal the value sees it: that it's weak says
something about it. Values with references aren't judged; a value written before this is judged at
its next write. The environment's list marks it "weak", and `roll secrets insights` prints `WEAK`. A change request's values (`…/requests/{id}/values`, for
reviewers who may reveal them) carry `proposed_weak` too, so a guessable credential is caught before
it's approved. The add and edit forms' "Generate" fills in 48 random
letters and digits made in the browser (rejection-sampled, like the CLI's `--generate`), so the fix
is one click.

### Health

`GET …/health` (`roll secrets health`, the dashboard's Health view) is every finding above in
every standard environment and folder at once, worst first, so none waits on someone opening the
right folder: `decoy_used` (a decoy key's credential was used; only the project's admins see it, see
Decoys), `expired`, `rotation_failing` (a managed key's scheduled rotation fails: the
database, vendor, product or rotator refused or didn't answer; the detail says why, the old
credential keeps working, and the sweeper tries again; cleared by the next rotation that works),
`revoke_failing` (a credential roll retired, or a lease that ended, that it couldn't revoke yet: it
still works until roll can; cleared once it does; `GET …/rotations`, `roll secrets rotations` and
the Health view's "Managed credentials" list every managed credential with its schedule and last
failure; `roll secrets rotations --rotate-due 7` rotates every one due within a week, or failing,
in one run, for an operator's maintenance window, and fails if one does), `missing` (a required key an environment doesn't define, through its
imports and base), `exposed` (a protected value also in an unprotected environment), `public`,
`weak`, `expiring`, `certificate_expiring` (a certificate a project CA signed for a CSR that ends
within two weeks: links' certificates are replaced by roll, a CSR's only by whoever asked; `key` is
its names, `path` its CA), `operator_made` (a sync or a workload identity made as the
operator account: "made by the operator account: re-save to bind it to a person". Before makers
were people, an admin's session made them as `operator`, which would act forever after the admin
was demoted or disabled; nothing is migrated, since nothing tells those from what the deployment's
own token made. Making the sync again, or the identity (or setting its pinned keys again), from a
person's session binds it to them. `key` is its id, an identity's environment and folder empty),
`stale`, then `unread` (nobody revealed, exported, resolved or synced it in 90 days,
and it's older than that). Each `Finding` names the environment, folder and key where it's set,
with a detail (the other environments, the reason, the date): never a value, and nothing is
decrypted. Only what the caller may read is listed, and `weak` only where it may reveal.

**In the browser.** `public` is a server's credential under a name a framework puts in the
JavaScript it sends to every visitor: `NEXT_PUBLIC_` (Next.js), `NUXT_PUBLIC_`, `EXPO_PUBLIC_`,
`REACT_APP_`, `VUE_APP_`, `GATSBY_`, `STORYBOOK_`, `VITE_` and `PUBLIC_` (SvelteKit, Astro). It's
a credential when its value has a server-only shape (`sk_live_`, `whsec_`, `ghp_`, `xoxb-`,
`SG.`, a private key, a connection string with a password…), when it links a field its provider
marks secret (a database's password or connection string), or when its name says so (`SECRET`,
`PASSWORD`, `PRIVATE_KEY`, `SERVICE_ROLE`), as does a reference to such a key
(`NEXT_PUBLIC_X=${STRIPE_SECRET_KEY}`). Keys made to be public aren't flagged: Stripe's
`pk_live_`, a Firebase or Maps browser key, Supabase's anon key. The write that sets one warns
(`warnings` on the answer), and health lists it with the framework and what it holds
(`NEXT_PUBLIC_ keys go into the browser bundle (Next.js), and this one holds a Stripe secret key`).
What kind of credential it is comes from the value, so a caller who may not reveal it (and the
daily notice's channel) sees "a server credential" instead.
Judged when a value is written, as `weak` is (a value written before this is judged by its name
and link only), so health decrypts nothing.

**Keys no code reads.** With `?code=1` (`roll secrets health --code`, the view's "Check the code"),
health also lists `unreferenced` keys: ones no repository on the project's canvas mentions on its
default branch, looked up in roll code's search index (rolld's `CodeRefs`; roll secrets reads no
files). A mention anywhere counts, as a case-sensitive substring, so a key that might be read is
never flagged. It's a search per key and repository (up to 500 keys), so only on request, and any
repository that can't be searched here (the index not ready, roll code off) makes the check say
nothing at all rather than guess. Whether a name appears in a repository says something about its contents,
so the check is only for a caller who may read every repository on the canvas (`Grants::reads_code`:
with roll-access, the caller's Grant for `code` read on that repository; until then, the operator):
anyone else gets a 403 that doesn't name it (that would tell a caller who can't see the canvas
the repository is there). In the
dashboard each finding fixes where it's listed: a weak key's "New value" (48 random characters
made in the browser) and an expired or expiring key's new date, each one change, or a change
request where the environment is protected. "New values for all N in Staging" does every weak key
of one environment at once: one change (or one change request) with a random value for each. The
Health tab and the sidebar's Secrets entry carry
the count of findings to fix now (expired, missing, exposed), and Home lists them across every
project (`GET /v1/secrets/findings`, `roll secrets health --all`).

Health, `POST …/leaks`, `GET /v1/secrets/search` and `GET /v1/secrets/findings` filter each
finding by the caller's own access: roll secrets asks the caller's roll-access Grant, so
roll-access lets a token limited to some environments or folders call them (health and leaks are
`filters_itself` in its catalogue, search and findings take any grant) and its answer just leaves
the rest out. Keep the filter in each: without it, the mark would let such a token see the whole
project.

### Compare and promote

- `GET …/compare?from=development&to=staging&path=/` lists every key in either environment with a
  status: `same`, `different`, `only_from` or `only_to`. Fingerprints decide; no value is revealed.
  A key in both that the caller can't compare (it may not reveal one side, or one is sensitive) is
  `hidden`: equality would test guesses against a value it can't read.
- `POST …/environments/{to}/promote?from={from}` copies the selected keys (values, comments, tags and
  links) from one environment to another as one change in the target. On a protected target, it
  opens a change request instead.

## Links

A **link** makes a secret's value come from a resource in the same project. The resource must be on
the project's canvas (a workspace resource). Each resource kind has a **link provider**: a small
trait in `crates/roll-secrets/src/links.rs`, implemented in `crates/rolld/src/secrets.rs` by calling
the product's own HTTP API in process. `roll-secrets` has no list of kinds: a product makes its
resources linkable by registering a provider. That is how roll search, roll apps and future
products join without changes here.

A live link's value changes however its credential does: roll rotating it, a password reset or
restore through the product, `ALTER ROLE` over SQL. Watches, syncs, roll apps' redeploys and
conditional reads follow the folder's digest, so the digest counts each live value's keyed
fingerprint as the product answers, read at most every 10 seconds (only the fingerprint is kept,
never the value). It moves at once, on every node, when roll rotates the credential and when a
product's own password, reset or restore route succeeds (rolld tells roll secrets:
`Secrets::source_changed`).

```rust
pub trait LinkProvider: Send + Sync {
    fn kind(&self) -> &str;                                   // "postgres", "search", …
    fn fields(&self) -> &[Field];                              // url, host, password, api_key…
    fn read(&self, r#ref: &str, field: &str, opts: &Opts) -> Result<Value>;          // live
    fn issue(&self, r#ref: &str, opts: &Opts) -> Result<Issued>;                    // managed/dynamic
    fn revoke(&self, r#ref: &str, handle: &Handle) -> Result<()>;
}
```

A link is `{kind, ref, field, mode, options}`:

- **`live`** (a connection URL, a host, a bucket's endpoint): the value is read from the product
  when it is resolved, so it is always current. If the product rotates the password, the next read
  has the new one. History records the link, not the value.
- **`managed`** (an access key pair, a stream API key): roll secrets asks the product to issue a
  credential, stores it as the secret's value (encrypted, versioned), and can **rotate** it, on
  demand or every `rotate_days`. Rotation issues a new credential, makes it the current version, and
  revokes the previous one after `grace_minutes` (default 60), so running services have time to
  pick up the new value. With `rotate_window` (as AWS Secrets Manager's schedule window: `"sun
  02:00-04:00"`, `"mon,thu 22:00-01:00"`, `"daily 03:00-05:00"`; UTC, at least an hour, ending past
  midnight on the next day), a due rotation waits for the window to open and runs inside it, so a
  credential changes only when you expect it to; `next_due` says when that is. A window goes with
  `rotate_days`, on a managed link.
- **`dynamic`** (short-lived credentials): each resolve that asks for it (`roll run`, an app's
  start) issues a fresh credential with a TTL (`ttl_minutes`, default 60, at least 15 minutes, at most 7 days), recorded
  as a **lease**. Leases are revoked when they expire, or at once with `DELETE …/leases/{id}`. A
  long-running consumer renews instead of restarting: `POST …/leases/{id}/renew` pushes the expiry
  out by the link's TTL (at most 30 days from issue), and the provider follows where the product
  enforces its own expiry (a Postgres role's `VALID UNTIL`). `roll run --watch` renews its leases
  every 10 minutes. `Values.dynamic` names the keys that hold one, for a consumer that can't renew
  (a service that scales to zero refuses them). Only
  the lease's metadata is stored, never the credential.
- **When a secret, an environment or the project goes**, what it issued is revoked first, while
  the project's key and registries (rotators…) can still reach it. A revocation that fails (the
  product or the customer's endpoint is down) stays as a lease that's already due, and the sweeper
  retries it; a deleted project keeps its key and registries (`projects.forgotten_at`) until its
  last lease is revoked, then they go too. Nothing it issued outlives it unrevoked.

**Suggested links.** `GET …/suggestions` lists each resource on the project's canvas with the
standard environments that neither link it nor have its usual keys at `/`, and those keys: the
provider's usual fields (a credential of the app's own, managed, wherever the product issues one:
a Postgres role, a bucket key, a Kafka user; never the shared password) under their usual names
(`DATABASE_URL`, `AWS_*`, `KAFKA_*`), prefixed with the resource's name when the canvas has two of a
kind. The overview offers each as one click to link it in all of them (a protected environment gets
a change request). Only roll can: it knows the canvas.

Which fields and modes each kind offers is in `GET …/links` (the catalogue of the project's
resources and their fields). The providers rolld registers are listed in `crates/rolld/src/secrets.rs`
and in the table below.

| Kind | Live fields | Issued (managed or dynamic) | Rotate the resource |
| --- | --- | --- | --- |
| `postgres` | `url`, `host`, `port`, `user`, `password`, `database` (option `branch`, default `main`) | a role of its own, credential `role`: `role_url`, `role_user`, `role_password` (`CREATE ROLE … VALID UNTIL` through the console's query route; option `permission` `read` or `write`) | no |
| `redis` | `url`, `host`, `port`, `password`, `rest_url`, `rest_token` | no (ACL users don't survive an unload) | yes: a new password |
| `stream` | `bootstrap`, `security_protocol` | an API key, credential `key`: `username`, `password`, `properties` | no |
| `bucket` | `bucket`, `endpoint`, `region` | an access key limited to the bucket, credential `key`: `access_key_id`, `secret_access_key` (option `permission` `read`, `write` or `admin`) | no |
| `nosql` | `cql`, `username`, `password`, `http_token`, `alternator` | no | yes: a new password |
| `queue` (a roll queues vhost, by name) | `host`, `port`, `vhost` | a user of its own on that vhost only, credential `user`: `url` (the AMQP URL with it), `username`, `password` (`PUT /v1/queues/users/{name}` with no tags, so no management sign-in, then `PUT /v1/queues/permissions/{vhost}/{name}`; option `permission` `read` (consume), `write` (publish and consume) or `configure`, the default: also declare and delete queues, exchanges and bindings). Rotating makes a new user and deletes the old one after the grace; revoking deletes it, which closes its connections. roll queues enforces no expiry of its own: a dynamic user lives until its lease is revoked | no (the vhost's own user is never readable: `…/credentials` shows a password once) |

Secrets of one environment linked to the same resource and credential share one issue: linking
`AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY` gives one access key, and rotating either rotates
both, in one change. A Postgres role made for a branch also exists in branches made from it later
(roles live in the branch's data); it expires with `VALID UNTIL` either way. The role gets a SCRAM-SHA-256
verifier, never the password itself, so a failing statement can't put it in the compute's log.
Revoking a role ends
its sessions and gives what it made (a migration's tables) to `roll` before dropping it, so revoking
a credential never drops data.

| `search` | `url`, `host`, `port`, `username` | a credential of its own with data access only (no keys or users; `"access": "data"`: a key roll search doesn't say is a data one, whatever else it says or if it says nothing, is deleted and refused), credential `key`: on an Elasticsearch cluster `api_key` (`Authorization: ApiKey …`), on an OpenSearch one an internal user, `user` (`<user>@<cluster>`, which the node routes) and `password`; a link to the field the cluster's flavor doesn't issue is refused when written, and switching a cluster's flavor carries no credential across (link the other fields again); an OpenSearch cluster's fields are offered and suggested under the names its clients read (`OPENSEARCH_URL`, `_HOST`, `_PORT`, `OPENSEARCH_USERNAME`/`_PASSWORD` for its user, `OPENSEARCH_CLUSTER` for the cluster's name); a dynamic one expires by itself 5 minutes after its lease | no (never the cluster's password) |

`search` resources are linkable once roll search's workspace kind lands (`feature/roll-search`).
roll apps' services have a workspace kind (`service`, `svc_…`); they consume secrets (an app's
variables, `app:<svc>`) rather than issue credentials, so they have no link provider. Products
without a link provider yet (tasks) join by registering one. Queues have no owners, so a person's
project stands for a queue only when the person is an admin (`Workspace::trusts`), as for
Postgres, Redis, streams, buckets and NoSQL.

### Rotators

Some values live outside roll and still need rotating: a vendor's API key, a database roll doesn't
run. A **rotator** is a URL the project names (`POST …/rotators {name, url}`, `admin`; the answer
holds its signing secret, once), and a link kind like a product's: a key linked to it in `managed`
mode (`{kind: "rotator", ref: "rot_…", field: "value", mode: "managed"}`, from the dashboard's Link
a resource too) gets its value from the rotator and then behaves as every managed link does. It
rotates every `rotate_days` or on demand, the old value is revoked after `grace_minutes`, keys of one
environment linked to the same rotator share one value, and a protected environment gets a change
request.

Each call is a POST from rolld's guarded client (HTTPS, a public address, no redirects), signed as
every roll webhook is (`X-Roll-Signature-256`, `X-Roll-Timestamp`, `X-Roll-Delivery`), with
`X-Roll-Rotator: <id>`:
- `{"action": "issue", "rotator", "label"}`: mint a new value; answer `{"value": "…", "handle": …}`
  (at most 96 KiB). `handle` is anything the endpoint needs to revoke that value later; it's kept
  as given (not sealed, like a product's credential handle), so it names the value, never holds it.
- `{"action": "revoke", "rotator", "handle"}`: retire it; any 2xx.

The URL (which may carry a token) and the signing secret are sealed with the project's data key;
only the host is shown. An answer is read once, never logged, and never part of an error. A rotator
a key still links to can't be deleted, nor one with a retired credential still waiting out its
grace period (its revocation goes through the rotator).

A rotator is edited in place (`PATCH …/rotators/{id} {name?, url?}`, the pencil; a new URL is
checked as adding one is) and its signing secret rotated (`POST …/rotators/{id}/rotate-secret`,
the key button): the new `whsec_…` is shown once and signs every call from then on. Put it in the
endpoint right away; a call the endpoint refuses in between fails, changes nothing (the value in
use stays) and is retried, as any failed rotation or revocation is.

### SaaS rotators

For the vendors below, roll speaks the vendor's own API, so there's no endpoint to write. A project
registers a vendor credential roll may issue and revoke keys with (`POST …/saas {name, vendor,
target, credential}`, `admin`; `roll secrets saas add NAME --vendor sendgrid`, the credential on
stdin), and a key linked to it in `managed` mode (`{kind: "saas", ref: "sas_…", field: "secret" |
"id", mode: "managed"}`, from the dashboard's Link a resource too) gets a fresh vendor secret on
every rotation. It works as a rotator does: it rotates every `rotate_days` or on demand, and the
old key is revoked after `grace_minutes`. `secret` is the vendor's secret (masked) and `id` is
the key's or secret's id at the vendor (plain). Keys of one environment linked to the same
registration share one issue: linking `TWILIO_API_KEY` to `id` and `TWILIO_API_SECRET` to `secret`
gives one Twilio key.

Registering checks the credential with the vendor first, read only, so a credential the vendor
refuses, or one that can't manage keys, is refused at once (400, naming the vendor and the status)
instead of at the first rotation:
- SendGrid: `GET /v3/scopes`, which must include `api_keys.create` and `api_keys.delete`;
- Twilio: one page of the account's keys;
- Okta: the app's secrets;
- Entra: sign in, then the application;
- Auth0: sign in, then the client.

| `vendor` | `target` | `credential` | Issue | Revoke |
| --- | --- | --- | --- | --- |
| `sendgrid` | `{scopes?: string[]}` (none: full access) | an API key that may manage API keys | `POST https://api.sendgrid.com/v3/api_keys {name, scopes?}` (Bearer) → `api_key`, `api_key_id` | `DELETE /v3/api_keys/{id}` |
| `twilio` | `{account_sid: "AC…"}` | the Auth Token, or a main API key as `SK…:secret` | `POST https://api.twilio.com/2010-04-01/Accounts/{sid}/Keys.json`, form `FriendlyName` (basic auth) → `sid`, `secret` | `DELETE …/Keys/{sid}.json` |
| `okta` | `{domain: "acme.okta.com", app_id}` | an API token (`SSWS`) that may manage the app | `POST https://{domain}/api/v1/apps/{app_id}/credentials/secrets` → `id`, `client_secret` | `POST …/secrets/{id}/lifecycle/deactivate`, then `DELETE …/secrets/{id}` |
| `entra` | `{tenant, client_id, object_id}` | a client secret of `client_id`, an app with `Application.ReadWrite.OwnedBy` that owns `object_id` (or `Application.ReadWrite.All`) | a client credentials token at `https://login.microsoftonline.com/{tenant}/oauth2/v2.0/token` (scope `https://graph.microsoft.com/.default`), then `POST https://graph.microsoft.com/v1.0/applications/{object_id}/addPassword {passwordCredential: {displayName}}` → `keyId`, `secretText` | `POST …/removePassword {keyId}` |
| `auth0` | `{domain: "acme.us.auth0.com", client_id, app_client_id}` | the client secret of `client_id`, a machine-to-machine app of the Management API with `update:client_keys` | a client credentials token at `https://{domain}/oauth/token` (audience `https://{domain}/api/v2/`), then `POST https://{domain}/api/v2/clients/{app_client_id}/rotate-secret` → `client_secret` (`id` is the app's client id) | nothing |

- **Auth0 has no grace period.** `rotate-secret` replaces the app's secret at once: the old one
  stops working the moment roll rotates, whatever `grace_minutes` says, so services must pick up
  the new value right away. `domain` is the tenant's Auth0 domain, not a custom domain (the
  Management API's audience names it), and `app_client_id` can't be the app roll signs in as.
- **One environment per Okta or Auth0 registration.** Auth0 keeps one secret per app and Okta two,
  so a second environment linking the same registration is refused (409 `in_use`): register the
  app once per environment, or link one environment. Okta also refuses a new secret while the app
  already has two; a rotation then fails and shows in health.
- Entra passwords get Microsoft's default lifetime (two years); set `rotate_days` below it.

Requests go out from rolld's guarded client, as a rotator's calls do: HTTPS to a public address, no
redirects, a 15-second timeout, at most 64 KiB of answer. The hosts are the vendors' own, except
Okta's and Auth0's tenant domains, which must be hostnames (no IP literal, no port) and then pass
the same guard. The credential is sealed with the project's data key, bound to the registration's
id, vendor and target, so a row pointed at another host no longer opens; it's never answered,
logged or audited, and the list shows the vendor and target only. A vendor's answer is never part
of an error: errors name the vendor, the step and the status (`okta answered 401 issuing a key`).
A key the vendor no longer has (404) counts as revoked. Deleting a registration is refused while a
key links to it or a retired key waits out its grace period, as with rotators.

### Databases outside roll

A Postgres, MySQL, SQL Server, ClickHouse, RabbitMQ, Redis, MongoDB, Elasticsearch, OpenSearch, Cassandra or ScyllaDB server roll doesn't run (Amazon RDS, Supabase, Neon, PlanetScale, ElastiCache, Upstash, Elastic Cloud, Amazon OpenSearch Service, a server of the customer's own)
has its logins rotated by roll itself, with no rotator to write, as Infisical's secret rotation
does, and issues a login per lease, as Vault's database engine does. A project registers it
(`POST …/outside-databases {name, url, roles?, group?, ca?}`, `admin`; `roll secrets databases add
NAME --role a --role b --group g`, the URL on stdin): `url` is an admin connection
(`postgres://admin:…@host:5432/db`, `mysql://admin:…@host:3306/db`,
`sqlserver://admin:…@host:1433/db`, `clickhouse://admin:…@host:8443/db`,
`rabbitmq://admin:…@host:15671/vhost`, `elasticsearch://admin:…@host:9200`,
`opensearch://admin:…@host:9200` or `cassandra://admin:…@host:9142/keyspace`, a user that may alter both
roles and create roles in the group), sealed with the project's data key like a rotator's (only `host:port` is shown); `roles` are
two login roles that take turns (managed links); `group` is a role whose grants dynamic logins get;
one or both. `ca` is the server's CA certificates (PEM) when the system's don't cover it (RDS).
Registering connects once and checks the roles exist (and can log in).

The admin credential rotates too, as Vault's `rotate-root` does (`POST …/outside-databases/{id}/rotate-admin`,
`roll secrets databases rotate-admin ID`, or the key button in the dashboard): roll sets a password
nobody chose on the user the admin URL logs in as (`ALTER ROLE CURRENT_USER`, MySQL's
`ALTER USER CURRENT_USER()`, SQL Server's `ALTER LOGIN`, Redis' `ACL SETUSER`, MongoDB's
`updateUser`, ClickHouse's `ALTER USER`, RabbitMQ's user API, Elasticsearch's
`_security/user/_password` as that user, OpenSearch's account API with the URL's password as the
current one, Cassandra's and ScyllaDB's `ALTER ROLE` on the URL's role, hashed where the server
takes it, then up to 5 s until the new password logs in past Cassandra's credentials cache; any
other URL is refused, never tried as Postgres) and keeps it sealed, so after it
nobody knows the admin password but roll. Whatever else uses that admin login breaks: give roll
an admin of its own. Redis' `default` user is refused (its password is every client's
`requirepass`), and so is a Redis server that can't keep an ACL change across a restart. The new
URL is stored before the server changes; a rotation whose answer is lost keeps whichever password
logs in, an interrupted one is finished on the next use, and an unfinished one is dropped only
once the stored URL is shown to log in, so roll never forgets the only working password. A
registration moves to another server (host or engine) only once no key links to it and no lease
is live: its logins are on the old one. A
registration is edited in place (`PATCH …/outside-databases/{id} {name?, url?, ca?}`, `roll
secrets databases set ID --name … --url < new-url`): a new URL is connected to and checked as
registering checks it before it's kept, and links, their roles and their leases carry on.

It's the link kind `outside_database` with fields `username` and `password`, one credential
(`login`), `managed` or `dynamic`.

**Managed** (two roles taking turns): each issue (linking, every `rotate_days`, `…/rotate`) gives the role not
handed out last a new 40-character password and hands it out, so the role in use keeps working
until its grace period ends; then its password is replaced by one nobody knows. Each role's
generation is kept with the database and carried in the credential's handle, so a retirement that
ends after the same role was handed out again does nothing. An issue never takes a busy role: the
one a stored credential uses, one an unrevoked lease still covers (the previous login in its
grace), or one another issue or retirement is changing (each holds its role for two minutes at
most), all worked out in the write that claims it. So two rotations racing (two nodes' sweepers, a
person and the schedule) can't reset the login in use; rotating again within the grace period is
a 409 until it ends. A rotation the server refuses changes nothing, and a role whose handout is
retired (its grace ended, or the write that issued it failed) is free again.
The two roles serve one credential, so a database is linked in one environment only (409 elsewhere;
a link provider says so with `exclusive()`): give staging and production a pair of roles each.

**Dynamic** (a login per lease): each resolve creates `roll_<random>` with `CREATE ROLE … LOGIN
PASSWORD <verifier> VALID UNTIL <lease end + 5 min> IN ROLE <group>`, so it has the group's grants
and the server refuses it after the lease even if roll is down; the username and password keys share
it. Renewing the lease moves `VALID UNTIL`. Ending it (expiry, `DELETE …/leases/{id}`) ends the
login's sessions where the admin may (`pg_signal_backend`) and drops the role; a role that owns
something the app made (a table) is kept, unable to log in (`NOLOGIN`, `VALID UNTIL '-infinity'`), for
the customer to reassign. Dynamic links work in any number of environments.

rolld connects the way it fetches anything a tenant chose: it resolves the host itself, refuses a
private address (`ROLL_CODE_WEBHOOK_ALLOW_PRIVATE` allows them for local development), and connects
to the address it checked. The URL may set `sslmode` and little else (no socket paths, several
hosts or `sslrootcert` files); TLS is required (`prefer` is treated as `require`, certificates are
verified) and `sslmode=disable` works only where private addresses do. The new password goes as a
SCRAM-SHA-256 verifier (`ALTER ROLE … PASSWORD 'SCRAM-SHA-256$4096:…'`), so neither the connection
nor the server's statement log ever holds it. One project can't register the same role on one host
twice (the two would undo each other's passwords), and a database a key links to, or with a
retired password still to replace, can't be deleted.

**MySQL** works the same way through the same guards (`mysql://…`; the URL may set `ssl-mode` and
nothing else; TLS verified against the URL's host, never a socket). Accounts are `'<name>'@'%'`:
rotation is `ALTER USER … IDENTIFIED BY` (the password in the statement, over TLS; MySQL rewrites it
out of its logs), and a dynamic login is `CREATE USER`, `GRANT <group>` and `SET DEFAULT ROLE
<group>`. MySQL has no `VALID UNTIL`, so a dynamic login lasts until roll ends its lease (renewing
changes nothing on the server); ending it kills its sessions where the admin may
(`CONNECTION_ADMIN`) and drops the account.

**Redis** (`rediss://user:password@host:6379`, ACL users; `redis://` only where private addresses
are allowed; no path or settings). Passwords go as `#<sha256>` (`ACL SETUSER <role> resetpass
#…`), so the server never sees one. Redis has no roles, so `group` names a template user (off, no
password) whose rules a dynamic login copies: `ACL SETUSER roll_… reset on #… <its commands, keys
and channels>`; a template with selectors is refused. Ending the lease is `ACL DELUSER`, which also
closes the user's connections; Redis has no expiry for a user, so a dynamic login lasts until roll
ends its lease. Every change is kept across a restart the way the server keeps its users: `ACL
SAVE` with an ACL file (a failure fails the change, or a restart would bring back a retired
password), else `CONFIG REWRITE` where there's a config file.

**MongoDB** (`mongodb+srv://user:password@cluster.example.net` or
`mongodb://user:password@host1,host2/?tls=true&authSource=admin`; `tls=true` is required unless
private addresses are allowed, and the URL may carry what Atlas-style strings do, nothing else)
works on a replica set or sharded cluster of the customer's own, or a host that lets the admin run
MongoDB's user commands. **It doesn't work on MongoDB Atlas, on any tier.** Atlas's "Unsupported
Commands" table marks `createUser`, `updateUser`, `usersInfo`, `grantRolesToUser` and
`revokeRolesFromUser` unsupported on Free and Flex, M10+ Core and M10+ Infinite clusters alike
(`dropUser` on M10+ only), and Atlas rolls back a user change made other than through its UI, CLI or
Admin API. A URL whose host is an Atlas domain (`*.mongodb.net`, `*.mongodbgov.net`) is refused when
it's registered, pointing to "MongoDB Atlas" below, which goes through the Admin API instead. User
management runs on the primary, found with `hello` among the seed hosts only: a server naming a
primary outside them is refused, and a `mongodb+srv://` name's SRV targets must sit inside its own
domain (the spec's rule), so a server or a DNS answer can't send roll anywhere else. roll logs in with
SCRAM-SHA-256, checks the server's signature (a server that can't prove it knows the admin's
password isn't the one the URL names), and refuses an iteration count past 1,000,000. Rotation is
`updateUser` with a new `pwd` over TLS (MongoDB keeps only SCRAM credentials and redacts them from
its logs); `group` is a role, `name` in `authSource` or `name@db` (`read@shop`); a dynamic login is
`createUser` with it, and ending the lease kills its sessions where the admin may
(`killAllSessionsByPattern`) and drops it. MongoDB has no expiry for a user. Replies are read by a
small BSON reader that checks every length against the bytes present, caps nesting, and reads at
most 1 MiB. The server is the tenant's, so its replies are read by a small parser that bounds
nesting, counts and lengths by the bytes present.

**ClickHouse** too, over its HTTPS interface (`clickhouse://admin:…@host:8443/db`, ClickHouse
Cloud's port; no settings): rolld pins the address it checked, verifies the certificate against
the URL's host (the system's roots and `ca`), follows no redirect and uses no proxy, and reads at
most 64 KiB of an answer. Values go as query parameters (`{n:String}`), never into SQL. Passwords
go as `IDENTIFIED WITH sha256_hash`, so the server never sees one (its query log holds the hash):
rotation is `ALTER USER` on two users that take turns; a dynamic login is `CREATE USER`, `GRANT
<group>` and `DEFAULT ROLE <group>` (the group is a role); ending it drops the user, then stops
what it's still running (`KILL QUERY`). ClickHouse has no expiry for a user, so a dynamic login lasts until
roll ends its lease. Users made over SQL need the admin's `access_management`; on a self-managed
cluster, keep users in replicated access storage. Checked against ClickHouse 25.8: rotation (the old
password refused, the new one taken), a dynamic login that signs in with the role and can't once
dropped, an untrusted certificate and a wrong admin password refused, and no password in the
server's query log.

**RabbitMQ** too, as Vault's and Infisical's RabbitMQ engines, through its management HTTPS API
(`rabbitmq://admin:…@host:15671/vhost`; no vhost is `/`; no settings), with the same guards as
ClickHouse (the checked address pinned, the certificate verified, no redirects or proxy, answers
read up to 256 KiB). The admin needs the `administrator` tag (checked at registration, with the
vhost and the users). Passwords go as RabbitMQ's own salted SHA-256 (`password_hash`), so the
broker never sees one. Rotation sets a new password on two users that take turns, keeping their
tags; `group` names a template user whose permissions and topic permissions on the vhost a
dynamic user copies (no tags), all of them or none: an exchange without a topic permission is open
to every routing key, so a template with more than 100 is refused rather than copied in part, and
a grant that fails undoes the user. RabbitMQ has no expiry for a user: ending a lease deletes it, then
closes its connections where the broker has that route (3.11 and later). Checked against RabbitMQ
4: rotation (tags kept, the old password refused), a dynamic user with the template's permissions
that's gone once dropped, and an untrusted certificate, a wrong password and a user without the
administrator tag refused.

**SQL Server** (and Azure SQL Managed Instance) too, as Vault's MSSQL engine (`sqlserver://…`; the
URL may set `encrypt=true` and nothing else). Everything is encrypted and the certificate verified
against the URL's host (TDS 7.4 with `Encrypt=Required`; nothing in plain text, even where private
addresses are allowed; TDS 8 `strict` isn't offered: SQL Server 2022 on Linux refused it in testing),
and a redirect to another server (Azure's gateway) is refused, since roll connects only to the
address it checked. Rotation is `ALTER LOGIN [name] WITH PASSWORD` on two SQL logins that take
turns; a dynamic login is `CREATE LOGIN`, then `CREATE USER … FOR LOGIN` and `ALTER ROLE <group> ADD
MEMBER` in the URL's database (the group is a database role). It has no `VALID UNTIL` either:
ending a lease disables the login first (that works while it's connected, so nothing can log in
with it even if a lingering session makes the drop fail and the sweeper retries), kills its
sessions, then drops the user and the login. Registering checks the admin login's server
permissions: `ALTER ANY LOGIN`, and for dynamic logins `VIEW SERVER STATE` and `ALTER ANY
CONNECTION`. roll's passwords mix upper- and lowercase letters and digits, which SQL Server's
password policy requires (a lowercase-and-digits one is refused, as checked live). A conversation reads at most 4 MiB from the server, on top of the
driver's own bounds. Checked against SQL Server 2022 in Docker with a certificate from a private CA:
rotation (the rotated login signs in), a dynamic login that signs in with the group's rights and
can't once dropped, and an untrusted certificate refused. With TLS 1.3 enabled on the server as
well (`tlsprotocols = 1.2,1.3`), SQL Server 2022 on Linux refused TDS 7.4 logins, Microsoft's own
`sqlcmd` included: such a server needs TLS 1.2 allowed.

**Elasticsearch and OpenSearch** too, as Vault's Elasticsearch plugin, through their security REST
APIs over HTTPS (`elasticsearch://admin:…@host:9200` or `opensearch://admin:…@host:9200`; Elastic
Cloud's `…es.io:443` works too; no path or settings), with the same guards as ClickHouse (the
checked address pinned, the certificate verified against the URL's host, no redirects or proxy,
answers read up to 64 KiB). Elasticsearch's users are the native realm's: rotation is `POST
_security/user/<name>/_password`, a dynamic user `PUT _security/user/<name>` with `roles:
[<group>]`, and ending the lease deletes it. OpenSearch's are the security plugin's internal
users: rotation is a JSON patch of the password alone (`PATCH
_plugins/_security/api/internalusers/<name>`), so the user's roles, backend roles and attributes
stay, and a dynamic user gets the group in `opendistro_security_roles`. `group` is a role. Both
clear a user's cached credentials when it changes, so the old password is refused at once and a
deleted user's next request fails; neither has an expiry for a user, so a dynamic one lasts until
roll ends its lease. The password goes in the request body over TLS; a refusal of a request with a
body is reported by its status and `error.type` alone, since Elasticsearch quotes request bodies
in some errors. The admin needs, on Elasticsearch, the `manage_security` cluster privilege (a role
with only that is enough; there's no narrower one for users), and on OpenSearch a user mapped to a
role in `plugins.security.restapi.roles_enabled` (`security_rest_api_access` on a default
install, or the domain's master user on Amazon OpenSearch Service). OpenSearch's reserved and
hidden users (its `admin`) can't be changed through the API, so they can't take turns. For example:

```sh
# Elasticsearch: a role for roll, two users that take turns, a role dynamic users get.
PUT _security/role/roll_admin   {"cluster": ["manage_security"]}
PUT _security/user/roll_admin   {"password": "…", "roles": ["roll_admin"]}
PUT _security/role/reader       {"indices": [{"names": ["app"], "privileges": ["read"]}]}
echo 'elasticsearch://roll_admin:…@es.example.com:9200' |
  roll secrets databases add search --role app_blue --role app_green --group reader
```

**Cassandra and ScyllaDB** too, as Vault's Cassandra engine, over CQL's native protocol v4 with
TLS (`cassandra://admin:…@host:9142/keyspace`; the keyspace is optional and checked at
registration; no settings). rolld speaks the protocol itself through nosql-proto's bounded frames
(capped at 256 KiB), on one connection to the address it checked, with the certificate verified
against the URL's host: no driver goes looking for the cluster's other nodes. Rotation is `ALTER
ROLE "<name>" WITH HASHED PASSWORD = '…'` on two roles that take turns, then up to 5 s until the
role logs in with the new password before it's handed out (Cassandra's credentials cache, warm from
an app's logins, refuses it for up to `credentials_validity`, 2 s by default; ScyllaDB takes it at
once), kept with a warning naming the role if it never does; a dynamic login is `CREATE
ROLE "<name>" WITH LOGIN = true AND HASHED PASSWORD = '…'` and `GRANT "<group>" TO "<name>"` (the
group is a role), dropped if the grant fails. Neither database takes a password as a bound value,
and both keep a traced statement whole (`system_traces.sessions`, for a client that asks or under
`nodetool settraceprobability`), so the statement holds rolld's bcrypt of the password, never the
password: `$2a$` at cost 10, Cassandra's default and the only prefix it takes (`$2b$` and `$2y$` are
"Invalid hashed password value. Please use jBcrypt."; ScyllaDB takes all three, at any cost).
Cassandra 5.0.9 takes the hashed form in both statements, from the narrower admin below too.
ScyllaDB 2026.3.3 takes it only from a superuser ("Only superusers can create a role with a hashed
password.") and only in `CREATE ROLE`: it answers `ALTER ROLE … WITH HASHED PASSWORD` with a server
error (`std::get: wrong index for variant`, nothing changed) even then. So a refusal, a server error
or a syntax error (a server without `HASHED PASSWORD`) sends the statement again with `PASSWORD =
'…'`, over TLS: on ScyllaDB rotations and a lease's end always go plain, and dynamic logins do
unless the admin is a superuser. Its audit log (`audit: "table"` with DCL, on by default) masks that as
`PASSWORD = '***'`, as Cassandra's audit and full-query logs do; its traces don't. roll's passwords
are letters and digits, so nothing needs escaping, and names are quoted (their case holds). A
refused statement holding a password reports a syntax error by its kind alone (the server quotes
the statement in one) and any other with the password, or its hash, cut out.
Neither has an expiry for a role: ending the lease gives the role a password nobody knows, then
`DROP ROLE IF EXISTS`. Cassandra caches each role's credentials on each node for up to
`credentials_validity` (2 s by default), so a login with an ended lease's password can still work
for that long, however the lease ends; set `credentials_validity` lower on the servers to shorten
it (ScyllaDB took a new password at once in roll's live checks). Registering checks that
both roles exist and can log in (`LIST ROLES OF`) and that the group exists; a server that lets
anyone in (`AllowAllAuthenticator`) is refused. The admin needs a superuser, or `CREATE`, `ALTER`,
`DROP`, `AUTHORIZE` and `DESCRIBE` on `ALL ROLES` and `SELECT` on `system_schema` for the keyspace check; the server needs `PasswordAuthenticator`,
and `CassandraAuthorizer` for the group's grants to mean anything. Differences seen: Cassandra 5.0
refuses a second password change on one role within 5 s (`Password for role … can only be changed
every 5000ms`; roll's turns are at least minutes apart, and a lease's end tries the unknown
password, then drops anyway); ScyllaDB 2026.3 makes no default `cassandra` superuser with a known
password (name one with `auth_superuser_name` and `auth_superuser_salted_password`) and its `LIST
ROLES` has no `datacenters` column. For example:

```sql
CREATE ROLE roll_admin WITH LOGIN = true AND PASSWORD = '…';
GRANT CREATE ON ALL ROLES TO roll_admin;  GRANT ALTER ON ALL ROLES TO roll_admin;
GRANT DROP ON ALL ROLES TO roll_admin;    GRANT AUTHORIZE ON ALL ROLES TO roll_admin;
GRANT DESCRIBE ON ALL ROLES TO roll_admin; GRANT SELECT ON KEYSPACE system_schema TO roll_admin;
CREATE ROLE reader;  GRANT SELECT ON KEYSPACE app TO reader;
-- echo 'cassandra://roll_admin:…@db.example.com:9142/app' |
--   roll secrets databases add cql --role app_blue --role app_green --group reader
```

`scripts/secrets-outside-check.sh [es] [os] [cassandra] [scylla]` checks all four live in Docker
(Elasticsearch 9.5.5, OpenSearch 3.9.0, Cassandra 5.0.9 and ScyllaDB 2026.3.3, each with
authentication on and TLS from a CA it makes): registering as the narrower admin above, refusals
for a missing user, role or keyspace, a wrong admin password (never echoed) and an untrusted
certificate; rotation (the new password logs in, the old one is refused, the user keeps its roles);
a dynamic login that reads with the group's grants and can't write or read elsewhere; and its
deletion (refused at once, twice is fine). For CQL it traces every request and checks what the
servers kept: no password roll sent in Cassandra's traces, none of a login the superuser made in
ScyllaDB's, and ScyllaDB's audit log masking the rest.

### Directories (LDAP and Active Directory)

An OpenLDAP or Active Directory server roll doesn't run has its accounts' passwords rotated by roll
itself, as Infisical's LDAP rotation does, and makes a user per lease, as Vault's LDAP secrets
engine does. A project registers it (`POST …/directories`, `admin`; `roll secrets directories add
NAME --url URL --bind-dn DN [--flavor active_directory] [--account DN…] [--user-base-dn DN]
[--group DN…] [--ca FILE]`, the bind password on stdin; or the dashboard, next to databases outside roll):

- `url` is `ldaps://host[:636]`, or `ldap://host[:389]`, which always starts with StartTLS: nothing
  is ever sent in plain text, even where private addresses are allowed. Nothing follows the host
  (no base DN, scope or filter).
- `bind_dn` and `bind_password` are the account roll binds as. The password is sealed with the
  project's data key, bound to the directory's row (only the DN is shown), and never returned,
  logged or audited. The bind account can't be one of the managed accounts (rotating it would lock
  roll out).
- `flavor` is `openldap` (the default) or `active_directory`.
- `managed_accounts` are one or two accounts' DNs (managed links); `user_base_dn` is where dynamic
  users are made and `groups` the groups they join (dynamic links). One or both.
- `ca` is the server's CA certificates (PEM), trusted as well as the system's: most domain
  controllers have a certificate from the domain's own CA. The certificate must name the URL's
  host in its subjectAltName (a name only in the subject's CN is refused): a Windows domain
  controller's from AD CS does, but the one Samba makes for itself doesn't, so give Samba one
  (`tls certfile`, `tls keyfile`).

Registering binds once, outside any transaction, and finds every account, group and the base (a
base search for each): a refusal is a 400 with the server's reason, never the password. One project
can't register the same account on one host twice (the two would undo each other's passwords).

It's the link kind `directory`, with fields `username` (the account's DN) and `password`, one
credential (`login`), `managed` or `dynamic`; the dashboard suggests `LDAP_BIND_DN` and
`LDAP_PASSWORD`. An Active Directory login also has `upn` (`LDAP_UPN`: its `userPrincipalName`,
else the implicit `sAMAccountName@domain` every account has, the domain being its DN's `DC=`
parts) and `account_name` (`LDAP_USERNAME`: its `sAMAccountName`), for applications that log in
with those rather than a DN; a managed account's are read before its password changes. Linking
them to an OpenLDAP directory is refused.

**Managed.** With two accounts, they take turns exactly as a database's two roles do (above): each
issue (linking, every `rotate_days`, `…/rotate`) sets a new password on the account not handed out
last and hands it out, so the one in use keeps working until its grace period ends; then its
password is replaced by one nobody knows. Rotating again within the grace period is a 409. **With one
account there is no grace:** each rotation replaces its password at once, so whatever still holds
the old one fails from that moment, and a second rotation within two minutes of the first is a 409
(the first is still being stored). Either way a directory's managed credential serves one
environment (409 elsewhere).

**Dynamic.** Each lease gets `cn=roll-<lease>,<user_base_dn>` with a random password, its
`description` naming the lease (`roll secrets lease lse_… (roll secrets staging LDAP_PASSWORD for
job:sync)`), and membership of every group (`member`). Ending the lease (expiry, `DELETE
…/leases/{id}`) deletes it; a user that's gone already counts as deleted. If making it fails midway
(a group refused it), what was made is deleted before the resolve answers. On Active Directory the
user is made disabled, given its password, then enabled; its `sAMAccountName` is `roll-` and 15
characters of the lease id, its `userPrincipalName` that name at `user_base_dn`'s domain, and its
`accountExpires` the lease's end plus five minutes, moved by each renewal, so the domain refuses it
after the lease even if roll is down. OpenLDAP has no expiry
for an entry: a user lasts until roll ends its lease. Checks, sandboxes and previews (untrusted)
never get a dynamic user. Dynamic links work in any number of environments.

**Passwords** are 32 characters with upper- and lowercase letters and digits; Active Directory's
also hold a symbol from `!#%+-.:=?@^_~`, for its complexity policy. On OpenLDAP a password is set
with the Password Modify extended operation (RFC 3062) where the root DSE lists it, so slapd hashes
it as configured (`olcPasswordHash`), else by replacing `userPassword`. On Active Directory it's a
replace of `unicodePwd` with the password in double quotes as UTF-16LE, which the domain accepts
only over TLS. A domain keeps taking an account's previous password for a while after a reset
(NTLM's old password allowed period, an hour by default; Samba accepts it for simple binds too), so
roll sets a throwaway password first and then the new one: the password it replaces stops working
at once, as above.

rolld connects the way it reaches a database outside roll: it resolves the host itself, refuses a
private address (`ROLL_CODE_WEBHOOK_ALLOW_PRIVATE` allows them for local development), connects to
the address it checked, and verifies the certificate against the URL's host. A conversation has 15
seconds, connecting included, and reads at most 1 MiB from the server; searches are base searches
for one entry.

The bind account needs, at least:

| Flavor | Managed accounts | Dynamic users |
| --- | --- | --- |
| `openldap` | `write` on each account's `userPassword` (`olcAccess: to dn.exact="cn=app-blue,…" attrs=userPassword by dn.exact="cn=roll,…" write`), and `read` on the root DSE's `supportedExtension` | `write` on `entry` and `children` of `user_base_dn` (to add and delete users) and on the new users' attributes, and `write` on each group's `member` |
| `active_directory` | the *Reset Password* extended right on each account (Delegate Control, "Reset user passwords and force password change at next logon") | *Create* and *Delete User objects* on the `user_base_dn` OU, *Reset Password* and write `userAccountControl` and `accountExpires` on its users (inherited, for `User` objects), and write `member` on each group. The attributes a user is made with (`description`, `sAMAccountName`, `userPrincipalName`) need nothing more: creating it covers them |

Checked against OpenLDAP 2.4 (`osixia/openldap:1.5.0`, a certificate from a private CA) over
LDAPS and StartTLS, bound as an account with only the `openldap` row's rights (`olcAccess` above):
the bind proof and a missing account refused, a new password that binds while the old one is
refused (stored `{SSHA}` through Password Modify), a dynamic user that binds and is in its group,
then deleted and out of the group, a second delete that's fine, and an untrusted certificate
refused (`a_real_openldap_sets_passwords_and_makes_and_deletes_users` in
`crates/rolld/src/secrets/directory.rs`, ignored: run by hand with `ROLL_E2E_LDAP_*`).

Checked against Samba's AD DC 4.22 (Debian trixie's `samba-ad-dc`, a domain provisioned with
`samba-tool domain provision`, its default policies) over LDAPS with a certificate from a private
CA, bound as an account given only the `active_directory` row's rights (`samba-tool dsacl set`,
the ACEs `CR` Reset Password on the account; `CCDC` for `User` on the OU plus inherited `CR` Reset
Password and `WP` on `userAccountControl` and `accountExpires`; `WP` on the group's `member`): the
bind proof, a missing account and a wrong bind password refused, a certificate from a CA that isn't
trusted refused (and Samba's own certificate, which has no subjectAltName); a managed account's new
password binds and its old one is refused at once, under Samba's default hour of old passwords; its
`sAMAccountName` and implicit UPN read, and the UPN binds; a dynamic user made disabled then
enabled (`userAccountControl` 512), with its `sAMAccountName`, UPN (which binds) and
`accountExpires`, in its group, its expiry moved by a renewal; one whose `accountExpires` has passed
refused; deleting it, and deleting it again
(`a_real_active_directory_sets_passwords_and_makes_expires_and_deletes_users`, ignored: run by hand
with `ROLL_E2E_AD_*`). Samba isn't Windows Server: not yet run against one are Windows' password
policy and fine-grained password policies with roll's passwords, its old-password period after a
reset, an AD CS certificate, LDAP signing and channel binding requirements, and replication
between domain controllers (a user made on one and bound on another right after).

A directory a key links to, or with a retired password or a dynamic user still to delete, can't be
deleted (409).
### Gateways (private networks)

A database or directory that only has a private address (inside a VPC, behind a firewall) is reached
through a **gateway**, as Infisical's and Akeyless' gateways reach them: a small process the team
runs inside the network. It dials out to roll over one WebSocket, and roll's conversations with
private hosts travel through it. Nothing listens inside the network, and the gateway decides what
it may reach, not roll.

**Records.** A gateway is a project resource (`gw_…`): `POST /v1/secrets/projects/{id}/gateways`
(`admin`; `roll secrets gateways add NAME`, or the dashboard's Gateways panel) makes the record and,
unless `"token": false`, its credential: a roll token (`rlt_…`, roll-access's) with one rule,
`{"product": "secrets", "resources": [{"id": "gw_…"}], "actions": ["write"]}`, shown once. That
rule reaches exactly one route, the gateway's own link; nothing else in roll names a gateway by id.
It's made through roll-access (`Access::create_grant`, as `POST /v1/access/tokens`), so it's
listed, revoked and audited with every other token, and roll-access looks it up on every connect.
roll makes it for people and the operator only, never for a token (a project-wide rule doesn't
cover a gateway's id), like identities. A gateway on Kubernetes, EC2 or in CI can instead log in
with its own OIDC token: a workload identity made with `"gateway": "gw_…"` mints exactly that rule
at each login (its environments, folders and actions are refused, they don't apply), for its
`ttl_minutes`; the gateway logs in again at four fifths of its token's life (at least a minute
before the end, timed on roll's clock from the login's `Date`) and opens a new link, which replaces
the old one: streams in flight finish on the old link, and any asked of it after the switch open on
the new one. Deleting or re-keying the identity drops the link at once. In the dashboard, the
identity form's "A login gets" picks the gateway. `GET
…/gateways` lists them with their status, `DELETE …/gateways/{id}` (`admin`; `roll secrets
gateways rm`) deletes one no database or directory uses (409 otherwise), revokes the token made
with it and closes its link at once.

**The binary.** `roll --url https://roll.example.com secrets gateways up --gateway gw_… --allow
10.0.0.0/8:5432 --allow ldap.corp.internal:636` (or `--allow-file FILE`, one entry a line, `#`
comments), with the token in `ROLL_GATEWAY_TOKEN` (or `--token-file`), or `--identity idn_…` and
the OIDC token in a file (`--oidc-token-file`, re-read at each login: Kubernetes' projected token,
`/var/run/secrets/…`; make the identity with `roll secrets identities add NAME --issuer … --subject
… --gateway gw_…`). It's a subcommand of `roll` (`crates/roll-cli`) beside the gateway's `ls`,
`add` and `rm` (`roll gateway` is roll code's model gateway already), over a small crate,
`crates/roll-gateway`, that holds the wire, the multiplexer, the allow-list and the agent loop:
rolld and the CLI share the wire, and the crate is tested on its own (no product in it). One
binary that's already on the machine is the simplest thing to run in a container (`roll secrets
gateways up` is its whole entrypoint), and it needs no project: its credential names the gateway. It opens one WebSocket to `GET
/v1/secrets/gateways/{id}/connect` and reconnects with jittered exponential backoff (0.5 s
doubling to 30 s, ±50%); a 4401 close (credential refused or gateway deleted) stops it, unless it
logs in through an identity, which logs in again first.

**The allow-list is the gateway's.** Each `--allow` is `HOST:PORT`, `*.SUFFIX:PORT` or `CIDR:PORT`
(IPv6 in brackets), where `PORT` is one port, a range `LOW-HIGH` (both included, 1–65535, `LOW <=
HIGH`: `10.0.0.0/8:5432-5439`) or `*`. An entry that doesn't parse stops the gateway at start, saying
which. The gateway refuses anything else before resolving
or dialing: a name must match a name entry, or resolve (on the gateway, with the network's DNS) only
to addresses inside a CIDR entry for that port; an IP literal must be inside a CIDR entry. Without
an entry it refuses everything. So a compromised or buggy roll can't use it to scan or reach the
rest of the network: it reaches what the team listed and nothing more. The gateway reports its list
in its hello (shown in the Gateways panel), but roll never widens or edits it. An SRV lookup
(`mongodb+srv://`) follows the same list: the gateway looks a record up only when the list could
allow a target of it (a CIDR entry, or a name or suffix entry under the record's parent domain), and
answers only when every target passes the check above; otherwise it refuses without naming the
target to roll, and logs the target itself.

**roll's own check stays.** A database or directory registered with `"gateway": "gw_…"` (a gateway
of the same project) may name a private address or a name only the network resolves: rolld doesn't
resolve it, and asks the gateway to open `host:port`. Every other path keeps the public-only rule
(`ROLL_CODE_WEBHOOK_ALLOW_PRIVATE` for local development only). TLS settings don't change with a
gateway: TLS still runs end to end from rolld to the server and is verified against the URL's host,
so the gateway relays ciphertext and never sees a password or a query.

**Wire.** Binary WebSocket messages, each `[type u8][stream u32 BE][payload]`, at most 64 KiB (both
sides set it as the WebSocket's message limit). The gateway's first message is `hello` (JSON:
version, its allow-list, its limits) and roll answers `welcome` (the node). Then roll sends `open`
(JSON `{host, port}`, 1 KiB at most) with a new stream id; the gateway answers `opened` or
`refused` (a reason: "not in the allow-list", "connection refused"), and both sides send `data`
(up to 16 KiB), `window` (credit), `fin` (end of that direction) and `reset`. roll may also send
`resolve` (JSON `{srv}`, an SRV name such as `_mongodb._tcp.cluster0.corp.internal`) with a new
stream id that never becomes a stream; the gateway looks it up with the network's DNS and answers
`resolved` (JSON `{targets}`, `host:port` each, 16 at most: more ends the link, and roll reads only
DNS names and ports from it) or `refused`. Lookups waiting count against a link's 32 streams. Pings
both ways every 20 s; 60 s of silence ends the link.

**Bounds.** Every stream has a 256 KiB window each way: a side sends only as many bytes as the other
granted, and grants more only as its reader takes them, so a slow reader holds at most a window,
and a peer that sends past it ends the link. Buffers grow with the bytes actually received, never
from a length the peer sent. A link carries at most 32 streams at once (the gateway refuses more),
each stream lives at most 20 s (a conversation's 15 s, connecting included, plus a margin) on both
sides, and `open` waits 10 s for an answer. Only the newest link of a gateway opens streams: a new
connect replaces the older one, which finishes its streams (20 s at most) and closes with 4409.

**Audit, both sides.** roll writes `gateway.connect` and `gateway.disconnect` rows (node, version,
the token's id) and a `gateway.stream` row per stream (the target `host:port`, and how it ended: bytes each way, or
the gateway's refusal).
The gateway logs one line per stream (`stream 7 open db.internal:5432 -> 10.0.3.4:5432`, `stream 7
closed: 1.2 KiB up, 3.4 KiB down` or `stream 8 refused 10.9.9.9:22: not in the allow-list`). Never
the bytes, a password or the token.

**Fleet.** Like an owner's agents (docs/code/cluster.md, "An owner's agents"), a gateway's link
lives on the holder of the lease `secrets:gateway:<id>`, from rolld's roll-cluster handle: the
connect is dispatched there (`Cluster::dispatch` forwards the upgrade), the link holds the lease's
guard, and when the lease moves the old holder closes the link with 1012 and the gateway reconnects
to the new one. A conversation that needs gateway X runs on X's holder: rolld dispatches the whole
provider call (`check`, `set_password`, `create_login`, `extend_login`, `drop_login`, and a
directory's) to the internal route `POST /internal/secrets/gateways/{id}/call`, its body sealed with
the deployment key every node shares (the admin URL and the new password are in it), so bytes never
stream between nodes and roll-cluster's 4 MiB forward limit holds. On one node all of this is a
no-op: the link and the call are local.
The internal route takes only a request another node signed (`roll_cluster::Forwarded`).
`secrets::gateway`'s `a_gateways_link_follows_its_lease_across_two_nodes` runs two nodes in one
process on one store and one in-memory roll-cluster backend, behind a load balancer: the link lands on
the lease's holder, a call made on the other node is dispatched there and rotates a password on an
HTTPS ClickHouse through the gateway, an unsigned or forged request to the internal route is
refused (403, 401), and when the holder drains the link closes with 1012 and the gateway comes back
on the other node, where calls work again.
`elasticsearch_and_cassandra_are_reached_through_the_gateway_and_only_on_its_allow_list` runs a
real gateway against an Elasticsearch-shaped HTTPS fake and a CQL fake over TLS: registering and
rotating through the gateway, the hashed CQL statements (never the password), TLS checked in rolld
with the database's CA, and a server the allow-list doesn't name refused by the gateway, never
dialed.

**Status.** Each link writes its status to the gateway's row (shared by every node): `connected`,
the `node`, `version`, the allow-list it reported, `connected_at`, `last_seen_at` (every 30 s while
up). It's online when connected and seen within 90 s.

**Revocation takes effect at once.** Before every stream it opens (each provider call through the
gateway asks for one), the link looks up its token through roll-access, as the connect does, and
the gateway's record, never cached. If either is gone (the token revoked or expired, its maker no
longer reaching the project, the gateway deleted), the stream is refused and the link closes with
4401. `DELETE …/gateways/{id}`, and deleting or re-keying the identity that logged a gateway in,
also re-check the link on the lease's holder right away, through the same dispatch as the calls
(`Call::Recheck`), so a link with no stream open closes too. A holder that can't be reached doesn't
fail the delete: the next stream's check covers it. An idle link still re-checks every 30 s, as a
backstop.

**Where it plugs in.** Databases outside roll (`gateway` on `POST …/outside-databases`: every
engine, Postgres, MySQL, SQL Server, ClickHouse, RabbitMQ, Redis, MongoDB, Elasticsearch,
OpenSearch, Cassandra and ScyllaDB, `mongodb+srv://` included) and directories (`gateway` on `POST
…/directories`). Postgres, SQL Server, Redis, MongoDB, Cassandra and ScyllaDB take the gateway's
stream directly (CQL's TLS runs in rolld over it, its frame and row caps as without a gateway).
ClickHouse, RabbitMQ, Elasticsearch and OpenSearch speak HTTPS through one small
client (hyper's, `outside/https.rs`) whose only connector is the conversation's way to the server:
a gateway's stream, or the address rolld resolved and checked, with TLS run in rolld against the
URL's host (SNI and verification, the engine's `ca`), no redirects, no proxy, answers read up to a
bound. For `mongodb+srv://` the gateway looks the SRV record up (a private cluster's name usually
resolves only inside the network); rolld still keeps only targets inside the record's parent
domain, then opens a stream to each host it needs. MySQL and LDAP clients take a socket, so rolld
hands them a loopback socket whose other end it relays to the stream, accepting only the connection
its own process made (directories pin their checked address the same way). `roll secrets
outside-databases add … --gateway gw_…`, `roll secrets directories add … --gateway gw_…`, and the
dashboard's forms offer the project's gateways.



### Snowflake

A Snowflake account has a service user's key rotated by roll, with no downtime, and makes a user
per lease, as Vault's Snowflake database plugin does. A project registers the account (`POST
…/snowflake-accounts`, `admin`; `roll secrets snowflake add NAME --account A [--privatelink]
--admin-user U [--admin-role R] --key FILE|- [--auth password] [--user U] [--role R…]`; or the
dashboard, next to directories):

- `account` is the account identifier: `myorg-myaccount`, or a locator such as
  `xy12345.us-east-2.aws`. Only letters, digits, `-`, `_` and `.` are accepted, with no
  `.snowflakecomputing.com`. Statements go to `https://<account>.snowflakecomputing.com`, or to
  `<account>.privatelink.snowflakecomputing.com` with `privatelink: true`. No other host is ever
  contacted.
- `admin_user` is the user roll signs in as, with key-pair JWT auth. `admin_private_key` is its RSA
  private key: PEM, PKCS#8 unencrypted or PKCS#1, 2048 to 4096 bits. The key is sealed with the
  project's data key, bound to the account's row, and never returned, logged or audited. Only its
  public fingerprint is shown (`admin_key_fp`, as `DESC USER` shows `RSA_PUBLIC_KEY_FP`).
  `admin_role` is the role statements run as (default: the admin user's default role).
- `auth` is how the credentials roll hands out sign in: `key_pair` (the default) or `password`, for
  users that can't use keys.
- `managed_user` is the user managed links rotate. It can't be the admin user, because rotating it
  would lock roll out.
- `roles` are the roles dynamic users may be granted, at most 16. `ACCOUNTADMIN`, `ORGADMIN`,
  `GLOBALORGADMIN` and `SECURITYADMIN` are refused.
- At least one of `managed_user` and `roles` must be set.

User and role names are names Snowflake reads unquoted: letters, digits, `_` and `$`, not starting
with a digit. roll upper-cases them and always quotes them in statements, so a role created with a
quoted lower-case name can't be used. No other text a tenant sends goes into a statement. A
`COMMENT` keeps only letters, digits and `_-.:#/@` and spaces.

Registering signs in once, outside any transaction. It runs `DESCRIBE USER` on the managed user and
`SHOW ROLES LIKE` for each role. A refusal is a 400 with Snowflake's code and message, never the
key. One project can't register the same managed user on one host twice, because the two
registrations would undo each other's keys.

It's the link kind `snowflake`, with one credential (`login`) of three fields: `user`, `account`, and
`private_key` (PKCS#8 PEM) or `password`, whichever the account's `auth` gives. A link to the other
one is a 400 when it's written. The links are `managed` or `dynamic`. The dashboard suggests
`SNOWFLAKE_USER`, `SNOWFLAKE_ACCOUNT` and `SNOWFLAKE_PRIVATE_KEY` (or `SNOWFLAKE_PASSWORD`). The
`account` field is the identifier as registered, with `.privatelink` appended when it's reached
over PrivateLink. That's what Snowflake's connectors take.

**Managed, key pairs.** A Snowflake user has two key slots, `RSA_PUBLIC_KEY` and
`RSA_PUBLIC_KEY_2`, and they take turns as a database's two roles do (above). Each issue (linking,
every `rotate_days`, `…/rotate`) makes a new 2048-bit key pair and sets its public half in the slot
not handed out last (`ALTER USER … SET RSA_PUBLIC_KEY_2 = '…'`). It then hands out the private
half. The other slot's key keeps working until its grace period ends, and then the slot is cleared
(`ALTER USER … UNSET RSA_PUBLIC_KEY`). Rotating again within the grace period is a 409.

If only the first slot holds a key when the account is registered (a key set by hand), roll's first
key goes in the second slot. The hand-set key keeps working until the second rotation.

**Managed, passwords.** One user has no second slot, so there is no grace: each rotation replaces
the password at once (`ALTER USER … SET PASSWORD = '…' MUST_CHANGE_PASSWORD = FALSE`). A second
rotation within two minutes of the first is a 409. A `TYPE = SERVICE` user can't have a password,
so the managed user must be `LEGACY_SERVICE` or `PERSON`.

Either way, an account's managed credential serves one environment (409 elsewhere).

**Dynamic.** Each lease gets the user `ROLL_<LEASE>` (for example `ROLL_LSE_…`). It is created by
these statements:

```sql
CREATE USER "ROLL_LSE_…" TYPE = SERVICE RSA_PUBLIC_KEY = '…' DEFAULT_ROLE = "LOADER"
  DAYS_TO_EXPIRY = 1 COMMENT = 'roll secrets lease lse_… (…)';
GRANT ROLE "LOADER" TO USER "ROLL_LSE_…";
```

With `auth: password` the user is `TYPE = LEGACY_SERVICE` with a random password. The roles are the
link's `roles` option, a subset of the account's (default: all of them), and the first is its
`DEFAULT_ROLE`. A link that names another role is a 400 when it's written.

`DAYS_TO_EXPIRY` is the lease's end plus five minutes, rounded up to whole days. Each renewal moves
it (`ALTER USER … SET DAYS_TO_EXPIRY`), so Snowflake refuses the user within a day of its lease
even if roll is down.

Ending the lease (expiry, `DELETE …/leases/{id}`) runs `DROP USER IF EXISTS`. If making the user
fails midway (a grant refused), the user is dropped before the resolve answers. Checks, sandboxes
and previews (untrusted) never get a dynamic user. Dynamic links work in any number of
environments.

**Passwords** are 32 characters with upper- and lowercase letters, digits and a symbol from
`!#%+-.:=?@^_~`, which fits Snowflake's default password policy and needs no quoting. A statement
that holds a password is never in an error: a refusal then says only Snowflake's code and SQLSTATE,
never its message. Snowflake's own query history may keep that statement's text. Key pairs put
nothing secret in a statement, so prefer them.

rolld sends each statement to the SQL API (`POST /api/v2/statements`) with a key-pair JWT for the
admin user (`X-Snowflake-Authorization-Token-Type: KEYPAIR_JWT`; it lives ten minutes). The request
goes through the same guarded client as webhooks and syncs: https only, a public address it resolved
itself (`ROLL_CODE_WEBHOOK_ALLOW_PRIVATE` allows private ones for local development), no redirects,
15 seconds per request, and at most 1 MiB read from an answer. A statement still running (202) is
polled once a second for up to 30 seconds. An account with a network policy must allow roll's
egress addresses for the admin user.

The admin role needs, at least:

```sql
CREATE ROLE ROLL_ADMIN;
GRANT ROLE ROLL_ADMIN TO ROLE SYSADMIN;  -- or wherever your role hierarchy keeps it
CREATE USER ROLL_ADMIN TYPE = SERVICE DEFAULT_ROLE = ROLL_ADMIN RSA_PUBLIC_KEY = '…';
GRANT ROLE ROLL_ADMIN TO USER ROLL_ADMIN;
-- Managed: DESCRIBE and ALTER the service user (its key slots or its password).
GRANT OWNERSHIP ON USER SVC_APP TO ROLE ROLL_ADMIN COPY CURRENT GRANTS;
-- Dynamic: make users (it owns them, so it can renew and drop them) and grant them its roles.
GRANT CREATE USER ON ACCOUNT TO ROLE ROLL_ADMIN;
GRANT OWNERSHIP ON ROLE LOADER TO ROLE ROLL_ADMIN COPY CURRENT GRANTS;
```

`GRANT ROLE` needs ownership of the role being granted. Owning the listed roles is the narrow way.
The alternative, `GRANT MANAGE GRANTS ON ACCOUNT`, lets the admin role grant any role, so avoid it.

The flow is tested against a fake SQL API that checks the JWT's signature and claims
(`a_snowflake_account_rotates_key_slots_in_turn_and_makes_a_user_per_lease`). The statements, the
JWT's `iss`/`sub` form, the SQL API's answers and the grants above follow Snowflake's documentation
and haven't been run against a real account yet.

An account a key links to, or with a retired key or a dynamic user still to drop, can't be deleted
(409).
### MongoDB Atlas

Atlas manages database users only through its UI, CLI and Admin API ("Databases outside roll"
explains why its clusters can't be reached the other way), so roll rotates an Atlas project's users
and makes a user per lease through the Admin API, as Vault's MongoDB Atlas database plugin does. A
project registers the Atlas project (`POST …/atlas-projects`, `admin`; `roll secrets atlas add NAME
--group-id ID --client-id ID [--user A --user B] [--role R…]`, the client secret on stdin; or the
dashboard, next to Snowflake):

- `group_id` is the Atlas project's id (24 hex digits; the Admin API calls it `groupId`).
- `client_id` and `client_secret` are an organization service account's (`mdb_sa_id_…`,
  `mdb_sa_sk_…`). The secret is sealed with the project's data key, bound to the registration's row,
  and never returned, logged or audited. Answers show only the client id.
- `managed_users` are two database users (password users on `admin`) that managed links rotate in
  turn. Both must exist when the project is registered.
- `roles` are what dynamic users get, as the Atlas CLI writes them: `role@db` or
  `role@db.collection` (`readWrite@shop`, `read@shop.orders`), built-in or custom, 16 at most.
  `atlasAdmin` is refused. Atlas checks a custom role when it creates a user.
- At least one of `managed_users` and `roles` must be set. One project can't register a user
  another of its registrations already rotates in the same Atlas project (409).

Registering takes a token and reads the managed users (or, with none, lists the project's users)
once, outside any transaction. A refusal is a 400 with Atlas's status and error code, never its
message or the secret.

**Calls.** The service account's token comes from `POST https://cloud.mongodb.com/api/oauth/token`
(client credentials: `Authorization: Basic <client_id:client_secret>`,
`grant_type=client_credentials`). It is kept in rolld's memory per registration until a minute
before its `expires_in` (an hour), never stored or logged, and a 401 from the API takes one new
token and retries once. A wrong secret is Atlas's `invalid_client`. The calls are:

| Call | Version (`Accept`, and `Content-Type` with a body) |
| --- | --- |
| `GET /api/atlas/v2/groups/{groupId}/databaseUsers?itemsPerPage=1` (registering with no managed users) | `application/vnd.atlas.2023-01-01+json` |
| `GET …/databaseUsers/admin/{username}` | `application/vnd.atlas.2023-01-01+json` |
| `PATCH …/databaseUsers/admin/{username}` (a password, or a dynamic user's `deleteAfterDate`) | `application/vnd.atlas.2023-01-01+json` |
| `POST …/databaseUsers` (a dynamic user) | `application/vnd.atlas.2023-01-01+json` |
| `DELETE …/databaseUsers/admin/{username}` (a lease's end) | `application/vnd.atlas.2023-01-01+json` |
| `GET …/clusters/{name}`, else `GET …/flexClusters/{name}` (a link's cluster) | `application/vnd.atlas.2024-08-05+json`, `application/vnd.atlas.2024-11-13+json` |

`databaseUsers` has one resource version in the Admin API's specification (the 2025-03-12 release):
`2023-01-01`. Atlas answers 406 to a version a resource doesn't have, so no 2025 date is sent there.
A `PATCH` sends the user back as Atlas returned it, with only the password (or the date) changed,
as Vault's plugin does, so its roles and scopes stay as they are. Every call goes through the same
guarded client as Snowflake's: HTTPS to `cloud.mongodb.com` only (no other host is ever built into
a URL), a public address rolld resolved itself, no redirects, 15 seconds a request, and at most
256 KiB read from an answer. An error carries Atlas's status and `errorCode`, never a request body
or Atlas's message.

It's the link kind `mongodb_atlas`, with one credential (`login`) of three fields: `username`,
`password` (32 letters and digits: the API calls the field alphanumeric, and nothing needs escaping
in a connection string) and `connection_string`. The links are `managed` or `dynamic`. Two options:

- `cluster` names a cluster (dedicated or Flex). `connection_string` is then its public
  `mongodb+srv://…` string as Atlas gives it (`connectionStrings.standardSrv`; no credentials), and a
  dynamic user can reach only that cluster (`scopes`). A `connection_string` link without it is a
  400, and so is a cluster Atlas doesn't know (checked when the link is written).
- `roles` (dynamic only) picks some of the registration's roles (default: all of them).

The dashboard suggests `MONGODB_USERNAME` and `MONGODB_PASSWORD` (and `MONGODB_CONNECTION_STRING`).

**Managed.** The two users take turns as a database's two roles do ("Databases outside roll"): each
issue (linking, every `rotate_days`, `…/rotate`) sets a new password on the user not handed out
last and hands it out. The user in use keeps its password until its grace period ends; then its
password is replaced by one nobody knows. Rotating again within the grace is a 409, and an Atlas
project's managed credential serves one environment (409 elsewhere).

**Atlas applies a user change asynchronously.** Its API answers at once, and the cluster takes the
change when Atlas deploys it, about a minute in practice; Atlas's documentation gives no figure. So a
new password can be refused for a minute or so after a rotation, while the old login keeps working
through the grace. The grace is 60 minutes by default and a managed link can't set `grace_minutes`
below 5. Something that reads the new value right after a rotation should retry a refused login for
a minute or two.

**Dynamic.** Each lease gets the user `roll_<lease>` (`roll_lse_…`), made by `POST …/databaseUsers`
with its password, the link's roles, `deleteAfterDate` (the lease's end plus ten minutes), a
`description` and the label `roll-secrets-lease`. So Atlas deletes the user even if roll is down.
Atlas takes a date at most a week ahead, so for a lease of nearly a week the date is capped at a week
from the call, less five minutes: Atlas may then remove the user a few minutes before the lease
ends. A renewal moves the date (`PATCH`). Ending the lease (expiry, `DELETE …/leases/{id}`) deletes the
user; one Atlas already removed counts as deleted. If making the user fails, it's deleted again
before the resolve answers. Like a new password, a new user can take a minute to work. Checks,
sandboxes and previews (untrusted) never get a dynamic user. Dynamic users count against the Atlas
project's database user limit (100 by default, 900 at most).

**The service account's role.** Give it the organization role Organization Member and, on the one
project, **Project Database Access Admin** (`GROUP_DATABASE_ACCESS_ADMIN`). The Admin API's
specification requires that role for creating, updating and deleting database users, and Project
Read Only for reading users and clusters; every project role includes Project Read Only. No
narrower role manages database users (Project Stream Processing Owner can too, but it's deprecated
and grants much more). If the organization requires an API access list, add roll's egress
addresses to the service account's: Atlas checks a token's use, not its creation, against the list.

The flow is tested against a fake Admin API that checks the token, the versions and the bodies
(`an_atlas_project_rotates_two_users_in_turn_and_makes_a_user_per_lease`). The calls, the token
flow, the versions and the role follow Atlas's documentation and OpenAPI specification and haven't
been run against a real Atlas account yet.

A project a key links to, or with a retired password or a dynamic user still to delete, can't be
deleted (409).



### GitHub App tokens
A short-lived GitHub token per lease, from the project's own GitHub App, as Akeyless and Vault's
GitHub plugin give: a release job gets `GITHUB_TOKEN` that reads one repository and opens pull
requests in it, and nothing else, for as long as the job runs. Nobody keeps a personal access token
in a secret.

A project admin registers the App's installation (`POST …/github-apps {name, app_id,
installation_id, private_key, repositories, permissions, api_base?}`, `roll secrets github-apps
add deploy --app-id 123 --installation 456 --repo web --permission contents:write < key.pem`, or
the Credentials tab): the App's private key, as GitHub gave it, is sealed with the project's key and
bound to its row, and never shown again. `repositories` and `permissions` are the most any token
may reach, as Vault and Akeyless put the ceiling on the role: a link narrows within them, never
past them, so an environment's admin can't mint a token the project's admin didn't allow, and no
token ever reaches every repository with all the App's power. roll gets one token with the
ceiling first, and revokes it at once, so a wrong id, key or a ceiling wider than the installation
is refused with GitHub's own reason. `api_base` names a GitHub
Enterprise Server's API (`https://github.example.com/api/v3`); rolld reaches it as it does every
URL a tenant chose (public addresses only, no redirects, at most 64 KiB read).

A key links to it `dynamic`, with field `token`. `options.repositories` names some of the App's
repositories (`["web"]`; none: all of the App's), and `options.permissions` maps permission names
to `read`, `write` or `admin` within the App's (`{"contents": "read"}`; none: the App's). Code
nobody vetted (checks, sandboxes, untrusted previews) never gets a token: an untrusted resolve
issues no dynamic credential. Each resolve signs a JWT as the App and gets an installation token narrowed that way
(`POST /app/installations/{id}/access_tokens`); the token is sealed in its lease, and the lease
ending (revoked, expired or swept) ends the token (`DELETE /installation/token`). GitHub ends one
after an hour on its own, so a link's `ttl_minutes` is 60 at most, and a lease can't be renewed:
resolve again for a new token. An App is deleted only once no key links to it and no lease holds a
token of it.
### GCP and Azure
Short-lived GCP and Azure access tokens for each lease, as Vault's GCP and Azure engines give, with
no key of the customer's stored anywhere: **roll is an OpenID Connect issuer**, and the customer's
cloud trusts roll's token for one project the way it trusts GitHub Actions' tokens.
- **The issuer** is `<rolld's public URL>/v1/secrets/oidc` (`ROLL_CODE_PUBLIC_URL`; the clouds must
  reach it over HTTPS). Its discovery document and keys (`…/.well-known/openid-configuration`,
  `…/jwks`) are public. Tokens are RS256 (Azure takes nothing else), live five minutes, and carry
  `sub = roll-secrets:<project>` (and `project`). The signing key (RSA 2048) is made at startup,
  sealed with the deployment key, and shared by every node; every published key verifies, and the
  public routes read only public halves. After a deployment-key rotation it's resealed like
  projects' keys (health's `resealed` counts it). The signing key itself rotates too (below,
  "Rotating the issuer's key"), and roll's other products get tokens of their own from it (below,
  "roll's tokens for other products"). It signs clouds' and apps' tokens, never the audit log's
  digests: those have a key of their own ("Tamper evidence", "The digest key"), so a leak of one
  forges nothing the other signs, and pulling one touches nothing the other vouches for.
- **GCP**: a workload identity pool provider for roll's issuer whose attribute condition pins
  `assertion.sub == "roll-secrets:<project>"`, and a service account that lets that principal
  impersonate it (`roles/iam.workloadIdentityUser`). For an app's instances, pin the whole
  `roll-apps:<project>:<service>:<env>` subject (or `assertion.environment == "production"` next to
  the service), never a prefix match. Register it with `POST …/cloud-accounts {name,
  cloud: "gcp", audience: "//iam.googleapis.com/projects/<number>/locations/global/workloadIdentityPools/<pool>/providers/<provider>",
  service_account}`. Each lease: STS token exchange, then `generateAccessToken` for the service
  account, for the lease's TTL plus five minutes (an hour at most).
- **Azure**: an Entra app with a federated credential for roll's issuer, subject
  `roll-secrets:<project>`, audience `api://AzureADTokenExchange` (for an app's instances, one
  federated credential per exact `roll-apps:…:<env>` subject; flexible credentials' wildcards on
  the environment are the same mistake as `*`). Register it with `{name, cloud:
  "azure", tenant, client_id, scope?}` (`https://management.azure.com/.default` by default). Each
  lease: client credentials with roll's token as the assertion.
- **The trust must name this project.** Registering an account (admin) first exchanges a token for a
  made-up project: one the cloud takes is refused, naming the condition to add, so no tenant can
  register (and use) another customer's account that trusts roll. Only a definite refusal
  (`invalid_grant`, `PERMISSION_DENIED`, `AADSTS70021`…) passes; a timeout or an outage fails the
  registration. Then this project's token must work.
- A key links to an account's `access_token` in `dynamic` mode, with a lease of 55 minutes at most
  (so it ends before its token; a longer one is refused when it's linked). A token can't be ended
  early or extended: a lease that ends leaves it valid until it expires, an hour at most; resolve again for a
  new one. An account a key links to can't be deleted. `roll secrets clouds ls` prints the issuer and
  subject to trust; `roll secrets clouds add-gcp` / `add-azure` register one.
#### roll's tokens for other products
Other roll products get tokens from the same issuer, in process only (no route signs one):
`Secrets::issue(subject, aud, ttl)`. The first is roll apps: each instance of a service gets one
(as Vercel's OIDC federation gives each deployment), so an app logs in to a cloud, or to roll
secrets, with no key of its own (docs/apps/README.md; apps delivers the token to its instances).
- **Subjects.** `Subject::App {project, service, env}` is `sub =
  roll-apps:<project>:<service>:<env>`, plus the claims `project`, `service` and `environment` for
  clouds' attribute mapping (GCP's `attribute.service = assertion.service`; AWS and Azure match
  `sub`). The project and the service are ids (a-z, 0-9,
  `_`, `-`), the environment a slug, so no part holds a `:` and a trust on one `sub` names one
  environment of one service. Other claims are `iss`, `aud`, `iat`, `nbf`, `exp` and a random
  `jti`.
- **Lifetime and audience.** A token lives the `ttl` asked for, an hour at most (a longer one is
  cut to an hour). The audience is the caller's choice (`sts.amazonaws.com`, a GCP pool provider,
  `roll`), 1–256 characters.
- **Only for a service its project stands for.** rolld's `ForApps::issue` (roll apps'
  `SecretSource::issue`) asks the workspace's trust, as a deploy's secrets do
  (`Workspace::trusts`): the operator's projects stand for their services, a person's only for
  their own. Otherwise it's an error, and no token.
- **Never for an untrusted preview.** `Secrets::issue` signs only for an environment the project
  has (not expired), and refuses an ephemeral one that isn't `trusted`: previews of a fork's pull
  request or other outside code (`pv-…`). Where a deploy's secrets give such a preview a narrowed
  read, the issuer gives it nothing, so fork code never holds a token that says roll vouches for
  the service. A trusted (vetted) preview gets one, under its own slug.
- **Trust the exact environment.** The `env` part of `sub` and the `environment` claim are the
  environment's real slug, so a preview's token never reads as production's. Write a cloud's trust
  on the exact subject (`roll-apps:<project>:<service>:production`), never
  `roll-apps:<project>:<service>:*`: a wildcard takes every trusted preview's token too. The AWS,
  GCP and Azure examples above say how.
- **Audit.** An app refreshes its token often, from every instance, so a row per token would bury
  the log: the first token per subject, audience and hour is an `oidc.issue` row in the project's
  log (actor `app:<service>`, the environment, the audience and the lifetime), and the rest of the
  hour only count (`oidc_issued`: subject, audience, hour, tokens; kept two days). The token is
  never logged.
- **Logging in to roll secrets with one.** A workload identity whose `issuer` is roll's own
  (`<public URL>/v1/secrets/oidc`) and whose `subject` is exactly one
  `roll-apps:<this project>:<service>:<env>` takes an instance's token: an app reads its project's
  secrets with no roll token in its settings. Its tokens are checked against roll's keys in
  process (roll's JWKS is never fetched over HTTP, and no `jwks` is pinned); the token's subject
  and its `project` claim must name the identity's project, so a subject of another project is
  refused when the identity is made and again at login.
#### Rotating the issuer's key
The signing key rotates every 90 days (`ROLL_SECRETS_OIDC_ROTATE_DAYS`, at least 3), run by the
sweeper:
- **Published before it signs.** When a rotation is due (the current key has signed for the
  period less two days), the next key is made, published in the JWKS at once, and signs from two
  days later (`issuer::PUBLISH_AHEAD_MS`): clouds cache JWKS (Azure for a day), and a token
  signed by a key they haven't fetched yet would be refused. The insert is the claim: it happens
  in a write that finds no key of roll's waiting, so one node makes it.
- **The old key stays until its tokens expire.** From then on the new key signs. The one it
  replaced stays in the JWKS for two hours more (`issuer::RETIRE_AFTER_MS`: the longest token, an
  hour, and an hour's margin), then leaves it (`retired_at`) and never signs again.
- **Retired keys still check legacy digests.** Before the digest key, the issuer key signed the
  audit log's digests. A retired key stays (sealed, as before), and the digest JWKS's `legacy`
  carries the public halves of the issuer keys that were roll's when the digest key was first made,
  so `audit verify` (and `--file`, with the digest JWKS alone) still checks the digests they signed.
  A key made since never counts: it never signed a digest.
- **Only roll's keys.** As before, only a row whose sealed private key opens with the deployment
  key and matches its public half signs, is published, or holds a rotation back.
- **Now.** `POST /v1/secrets/issuer/rotate` (the operator's; `roll secrets issuer rotate`) starts
  a rotation at once: the next key is published now and signs two days later, as a scheduled one.
  With a key already waiting, it answers that one. `{kid, signs_from}`.
- **An emergency.** For a key that may have leaked, `{"now": true, "retire": "<kid>"}` (`roll
  secrets issuer rotate --now --retire KID`; the operator's, and both or neither) makes a new key
  that signs at once and takes `<kid>` out of the JWKS in the same write (`retired_at`,
  `pulled_at`): tokens it signed stop verifying as soon as a verifier fetches the JWKS again, and
  it never signs again. A kid that isn't one of roll's keys in the JWKS is a 404. The cost is the
  two days' notice: a cloud that cached the JWKS refuses the new key's tokens until it fetches
  again (Azure: up to a day). `GET /v1/secrets/health`'s `issuer_pulled` names each key pulled in
  the last 30 days (`{kid, pulled_at, detail}`), and a cloud trust or verifier that pinned it
  rather than reading the JWKS must be given the new key. No digest rests on it: since the digest
  key it signs none, and in the same write it leaves the digest keys' `legacy` list, so the legacy
  digests it signed before vouch for nothing again. Each is covered by the next digest under the
  digest key (signed within the hour of the upgrade), so `audit verify` stays `ok`; it lists the
  pulled kid in `pulled`, `countersigned` by that digest.
- **Audited.** Each operator's rotation, either kind, is an `issuer.rotate` row (actor
  `operator`) in every project's audit log: the key signs for all of them.
### Kubernetes service-account tokens
A short-lived token of a Kubernetes service account for each lease, as Vault's Kubernetes secrets
engine gives, so a pod, a CI job or a person gets cluster access for minutes rather than a token
that never expires.
- **Register a cluster** (`POST …/kubernetes {name, api_url, ca_pem?, token}`, `admin`; `roll
  secrets kubernetes-clusters add NAME API_URL [--ca FILE]` with the token on stdin, or the
  dashboard's Credentials tab). `token` is a service account token roll manages the cluster with: for
  links to existing service accounts it needs `create` on `serviceaccounts/token` in the namespaces
  links name, and nothing more (a ClusterRole with that one rule, bound per namespace); links with
  `role_rules` need more (["Generated roles"](#generated-roles), below). `api_url` is the API server's `https://`
  URL. It's a URL the tenant chose, so every call goes through rolld's guarded client: public
  addresses only, no redirects, a 15 s timeout, a bounded answer. `ca_pem` is the cluster's CA
  (PEM), when its API server's certificate isn't publicly trusted (most self-managed clusters,
  EKS, GKE): then that cluster's calls trust only it, and nothing else's calls trust it. The token
  and the CA are sealed with the project's data key; the answer (`KubernetesCluster`) shows the
  URL and whether there's a CA, never either. Registering calls `GET /api` with the token first:
  the address, the TLS trust and the token must all work. A cluster a key links to, or with a
  lease still live, can't be deleted (409).
- **Link a key** to the cluster in `dynamic` mode (the only mode), naming in the options either an
  existing service account or the rules of a role made per lease: `{kind: "kubernetes", ref:
  "kube_…", field: "token", mode: "dynamic", options}`, where `options` are:
  - `namespace`: the service account's namespace (a Kubernetes name).
  - `service_account`: an existing service account there; or
  - `role_rules`: Kubernetes PolicyRules (`[{apiGroups, resources, verbs, resourceNames?}]`, 1 to
    32 rules, at most 32 entries per list, Kubernetes' name rules; `""` is the core group and `*`
    is allowed where Kubernetes allows it; nothing else in a rule, so no `nonResourceURLs`) for a
    service account made per lease ([below](#generated-roles)). `service_account` and
    `role_rules` are exclusive: a link with both, or neither, is refused when it's written (400).
  - `cluster_wide`: with `role_rules`, a ClusterRole and ClusterRoleBinding rather than a Role and
    RoleBinding (default `false`).
  - `audiences`: up to 10 audiences for the token (the API server's own when unset).
  - `ttl_minutes`: the lease's TTL (15 to 1440).

  Each resolve that asks for
  dynamic credentials calls the TokenRequest API (`POST
  /api/v1/namespaces/{namespace}/serviceaccounts/{service_account}/token`, `expirationSeconds` the
  lease's TTL, `audiences` when set, else the API server's own) and records a lease. The fields are
  `token` (the credential), and `api_url` and `ca` (plain, from the registration), so a pod or a
  CI job can build a kubeconfig; keys of one environment linked to the same cluster with the same
  options share one token. Code nobody vetted (checks, sandboxes, previews) never gets one, as with
  every dynamic link.
- **An existing account's token lives until its expiry.** A bound service-account token can't be
  revoked on its own: ending a lease (`DELETE …/leases/{id}`, or its expiry) records the
  revocation, and the audit row says the token still works until it expires. So keep `ttl_minutes`
  short: 15 minutes at least (roll's floor; Kubernetes' own is 10), a day at most, and leases don't
  renew (resolve again). roll never hands out a token the API server made to outlive its lease. To
  cut access at once, delete the service account or its RoleBinding in the cluster, or use
  `role_rules`. Errors name the cluster's host, the step and the status (with roll's hint for 401,
  403, 404 and 409), never a token or the API server's message.

#### Generated roles
With `role_rules`, as Vault's Kubernetes secrets engine's generated role rules, each lease gets a
service account of its own, and **revoking the lease ends its tokens**. Each issue:
1. creates a ServiceAccount `roll-<lease>` in `namespace` (`<lease>` is the lease id without its
   `lse_` prefix; `automountServiceAccountToken: false`);
2. creates a Role `roll-<lease>` there with the rules, or a ClusterRole if `cluster_wide`;
3. creates a RoleBinding `roll-<lease>` binding it to the account, or a ClusterRoleBinding;
4. asks TokenRequest for a token of that account, for the lease's TTL.

Every object is labelled `app.kubernetes.io/managed-by: roll-secrets` and `roll.sh/lease: lse_…`,
so leftovers can be found (`kubectl get sa,role,rolebinding,clusterrole,clusterrolebinding -A -l
app.kubernetes.io/managed-by=roll-secrets`). If a step fails, roll deletes what that issue already
made (outside any database transaction) and the resolve reports the step and the status
(`kube.example answered 403 to creating the RoleBinding roll-… in ci: the admin token may not do
this (it needs create on rolebindings)`), never a token; if a delete fails too, the error says so
and names the label. Revoking the lease (`DELETE …/leases/{id}`, its expiry, or its secret,
environment or project going) deletes the binding, the role and the account, in that order: the
permissions go first, then the account, and with it every token it had. A 404 counts as already
gone; any other failure keeps the lease due, and the sweeper retries it, as with every product.

The admin token then needs to create and delete ServiceAccounts, Roles and RoleBindings in the
namespaces links name. Kubernetes' escalation check also means it can only grant what it holds
itself, unless it has `escalate` (to create a Role with rules it doesn't hold) and `bind` (to bind
one) on `roles`. The minimal RBAC for roll's admin service account (here `roll-secrets` in
`roll-system`), bound in each namespace links name:

```yaml
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: roll-secrets
rules:
  # Tokens (every kubernetes link).
  - apiGroups: [""]
    resources: ["serviceaccounts/token"]
    verbs: ["create"]
  # Generated roles (role_rules links).
  - apiGroups: [""]
    resources: ["serviceaccounts"]
    verbs: ["create", "delete"]
  - apiGroups: ["rbac.authorization.k8s.io"]
    resources: ["roles", "rolebindings"]
    verbs: ["create", "delete"]
  # Grant rules roll's account doesn't hold itself. Leave this out and role_rules may only
  # name permissions roll's account has in that namespace.
  - apiGroups: ["rbac.authorization.k8s.io"]
    resources: ["roles"]
    verbs: ["bind", "escalate"]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: RoleBinding        # one per namespace links name
metadata:
  name: roll-secrets
  namespace: ci
roleRef:
  apiGroup: rbac.authorization.k8s.io
  kind: ClusterRole
  name: roll-secrets
subjects:
  - kind: ServiceAccount
    name: roll-secrets
    namespace: roll-system
```

Bound per namespace, `escalate` and `bind` reach only that namespace. `cluster_wide` links need
cluster-scoped permissions too, which make roll's account able to grant anything in the cluster;
give them only where that's intended:

```yaml
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: roll-secrets-cluster-wide
rules:
  - apiGroups: ["rbac.authorization.k8s.io"]
    resources: ["clusterroles", "clusterrolebindings"]
    verbs: ["create", "delete"]
  - apiGroups: ["rbac.authorization.k8s.io"]
    resources: ["clusterroles"]
    verbs: ["bind", "escalate"]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRoleBinding
metadata:
  name: roll-secrets-cluster-wide
roleRef:
  apiGroup: rbac.authorization.k8s.io
  kind: ClusterRole
  name: roll-secrets-cluster-wide
subjects:
  - kind: ServiceAccount
    name: roll-secrets
    namespace: roll-system
```

(The account itself still lives in `namespace`, so the namespaced ClusterRole above is bound
there too.) roll doesn't sweep leftovers on its own yet: a lease is revoked through its handle, and
the labels are for an operator's `kubectl delete -l roll.sh/lease=…`.
### Certificates
A project's own certificate authority, as Vault's PKI engine and Infisical's PKI give: short-lived
TLS certificates for services (mTLS between them), with no CA to run.
- `POST …/cas {name, domains, allow_ip?, max_days?}` (admin; `roll secrets cas add NAME --domain
  svc.internal`, the Keys & certificates tab) makes an ECDSA P-256 key and a self-signed certificate for ten
  years. The key is sealed with the project's data key, bound to the CA, and never leaves roll; the
  certificate (`GET …/cas`, `roll secrets cas cert NAME`) is what clients trust. It carries name
  constraints: only its domains, and IP addresses only where allowed (otherwise excluded), so a
  client that trusts it trusts nothing else.
- The link kind `pki` has `certificate`, `private_key` and `ca` (one credential, PEM), with the
  certificate's names in `options.names` (a list, or comma-separated; the dashboard's Link dialog
  asks): DNS names under the CA's domains, a wildcard one level under one, or IP addresses where
  allowed; anything else is refused, naming the domains. `dynamic` gives each lease its own
  certificate for the lease's TTL plus five minutes; `managed` stores one and rotates it every
  `rotate_days` (required, and at most half the CA's `max_days`; the Link dialog asks), valid for
  twice that, so a rotation always comes first. Both are at most `max_days` (90 by default, 397 at
  most) and never past the CA's own end. A link's names and rotation are checked when it's written.
  Certificates are for servers and clients both (`serverAuth`, `clientAuth`; the key only signs),
  with a random 128-bit serial.
- `POST …/cas/{id}/sign {csr (PEM), days? | ttl_minutes?}` (`write`, audited; `roll secrets cas
  sign NAME req.csr [--days N] [--chain]`) signs a CSR, for a device or a service mesh that keeps
  its own private key. The request's signature is checked, then only its public key and its
  subjectAltName names (DNS names and IP addresses, each within the policy; the subject's CN isn't
  read) are taken: usages, lifetime (30 days, or the CA's `max_days` if shorter, by default; 5
  minutes to `max_days`) and everything else are the CA's own, whatever the CSR asks (being a CA
  itself, signing code). The CSR's key must match its signature (a P-384 key signed with SHA-384,
  not SHA-256). It answers `{certificate, ca, serial, names, not_after}`. Checked with
  OpenSSL's CSRs (P-256 and RSA): `openssl verify -x509_strict` passes against the CA.
- Every certificate a CA signs is listed until it expires (`GET …/cas/{id}/certificates`: serial,
  names, `csr`, `link` or `acme`, its end, who asked, when it was revoked; `roll secrets cas certs NAME`).
  `…/cas/{id}/revoke {serial}` (`write`, audited; `roll secrets cas revoke NAME SERIAL`, the
  dashboard's Revoke) takes one back, a lost device's, say; roll revokes a lease's certificate
  when the lease ends and a managed one's when its grace runs out. The CA signs a CRL of the
  revoked certificates that haven't expired, good for an hour and signed at most once a minute per
  node (a revocation shows in that node's next one at once, another node's within a minute):
  `GET /v1/secrets/crl/{id}` (DER, `application/pkix-crl`; `?format=pem`), without a token, as any
  CRL is fetched. Every certificate names it in its CRL distribution point (rolld's
  `ROLL_CODE_PUBLIC_URL`). Checked with OpenSSL: `openssl verify -crl_check` refuses a revoked one
  (`certificate revoked`) and passes the rest. **OCSP** too, beside the
  CRL (RFC 6960), for clients that ask about one certificate rather than fetch lists:
  `POST /v1/secrets/ocsp/{id}` (the request's DER as the body) or `GET /v1/secrets/ocsp/{id}/<base64
  of the request>` (cacheable), without a token. A certificate is `good` while the CA lists it
  unrevoked, `revoked` with its time once revoked, `unknown` when the CA didn't sign it, it has
  expired or the request names another issuer; the CA signs each answer itself. The route is
  public, so its cost is bounded: nonces aren't echoed (RFC 8954 allows it, as large public CAs
  do), answers are kept a minute per node by the certificates asked about (a revocation drops the
  CA's), and a CA signs at most 50 fresh answers a second per node (`tryLater` beyond). An answer is
  good for ten minutes (`nextUpdate`; GET answers `max-age=60`, and `no-store` on anything but a signed answer): how long a revocation can take to
  reach a client that keeps answers. Every
  certificate names the responder (Authority Information Access). Checked with `openssl ocsp`:
  `good` and `revoked`, `Response verify OK`, over POST and over GET. Clients that
  don't check CRLs or OCSP still trust a revoked
  certificate until it ends, so lifetimes stay short. A CA a key links to can't be deleted, nor one
  with a revoked certificate still within its lifetime (its CRL must keep answering; a deleted
  project's CAs keep serving theirs until then too); deleting one leaves what it signed valid until
  it expires. One CA keeps at most 10,000 certificates from CSRs valid at once. Serials are
  random, positive and never start with a zero byte, so the serial a client shows is the one to
  revoke.

### ACME
Each CA is an ACME server (RFC 8555), as Vault's PKI is: cert-manager, certbot, Caddy and lego get
its certificates with no roll client and no roll token. Its directory is
`<ROLL_CODE_PUBLIC_URL>/v1/secrets/acme/{ca}/directory` (the CA's ACME panel, `roll secrets cas acme
eab ls NAME`).
- **External Account Binding, always.** An admin of the CA's project makes a binding (`POST
  …/cas/{id}/acme/eab {label?, names?, days?}`, audited; `roll secrets cas acme eab add NAME
  [--name D…] [--days N] [--label L]`, the ACME panel's New binding): a key id (`eab_…`) and an
  HMAC key, shown once and kept sealed with the project's data key. A binding is tied to its CA and
  optionally to narrower `names` (each takes itself and every name under it, within the CA's
  domains) and an end. An account is made only with a binding's MAC over its own key, and acts with
  that binding's names. Every request of an account looks its binding up, never cached: revoking it
  (`DELETE …/acme/eab/{id}`, `roll secrets cas acme eab rm NAME ID`, the panel's Revoke), its end,
  or its maker no longer reaching the project (as with tokens they made) stops the account at once.
  One binding makes at most 50 accounts (the same key in several clusters is fine); a CA has at most
  200 live bindings.
- **Names.** Every name of an order is within the CA's policy and the binding's names, or the order is
  refused (`rejectedIdentifier`). Then each is proven:
  - `http-01`: roll fetches `http://<name>/.well-known/acme-challenge/<token>` through rolld's
    guarded client: public addresses only (`roll_net::PublicOnly` and the literal-address check; a
    deployment with `ROLL_CODE_WEBHOOK_ALLOW_PRIVATE` reaches private ones), port 80, no redirects
    followed, no proxy, 10 seconds, 8 KiB read at most. Not for wildcards.
  - `dns-01`: roll looks up `_acme-challenge.<name>` TXT through public DNS (Cloudflare's resolvers,
    not the fleet's own, so a private zone can't answer for a name; the system resolver where
    private addresses are allowed), nothing cached, 10 seconds, 32 records read at most.
  - `tls-alpn-01` (RFC 8737): roll resolves the name through the same guard as `http-01` (public
    addresses only, unless `ROLL_CODE_WEBHOOK_ALLOW_PRIVATE`), connects to that address on port 443
    only and makes one TLS 1.2+ handshake with SNI the name and ALPN `acme-tls/1`, all within 10
    seconds. The server must choose `acme-tls/1`; its certificate (self-signed by design, so any is
    taken that parses and is 16 KiB at most, while the handshake's own signatures are still
    checked) must have exactly one subjectAltName, the name, and the critical
    `id-pe-acmeIdentifier` extension (1.3.6.1.5.5.7.1.31) holding an OCTET STRING of SHA-256 of the
    key authorization. Not for wildcards or IP addresses.
  - **Reused authorizations.** A new order for a name the account already proved reuses that
    authorization, as Let's Encrypt does (cert-manager expects it): the same account, CA and
    identifier, the same `wildcard`, valid, with an hour or more left. The order lists it in
    `authorizations` and is `ready` without a challenge. Reusing never extends an authorization (a
    day from when it was made); the order ends with the earliest of its authorizations.
  - **Pre-authorized names** (`PATCH …/cas/{id}/acme {pre_authorize: true}`, `roll secrets cas acme
    pre-authorize NAME on`, the panel's checkbox; off by default, audited): every name an account's
    binding takes is valid at once, with no challenge. Most internal names resolve to private
    addresses roll's fleet can't reach, so this is the mode internal clusters (cert-manager inside a
    VPC) use. **The binding's HMAC key is then the whole proof**: whoever holds it gets certificates
    for every name the binding takes. Give each cluster its own binding with only its names, and an
    end.
- **Finalize** first checks the order's names against the CA's policy and the account's binding again
  (`unauthorized` if either no longer takes one: an order, or an authorization it reuses, may
  predate the change; a revoked binding stops every request), then goes through the same path as
  `…/cas/{id}/sign`: the CSR's signature is checked and only its public key and names are taken
  (exactly the order's names); usages and lifetime (30 days, or the
  CA's `max_days` if shorter) are the CA's. The certificate is recorded with `source: "acme"` (the
  CA's list, its CRL and OCSP have it) and `issued_by: "acme:<binding>"`, and audited
  (`ca.acme.issue`, naming the binding and its maker) in the write that makes the order valid.
  `revoke-cert` takes a certificate issued over ACME, signed by the account that ordered it or with
  the certificate's own key (`ca.revoke`, audited); it's on the CRL at once.
- **What's checked.** Requests are flattened JWS signed with ES256 (P-256) or RS256 (2048 bits or
  more); `none`, HMAC and anything else are refused (`badSignatureAlgorithm`). The `url` in the
  protected header must be the request's own; each request carries a nonce roll made within the hour
  (a MAC with a deployment-wide key, so handing one out writes nothing) and used once (stored until it
  would expire). `key-change` takes the old key's request wrapping the new key's. A request is 64 KiB
  at most, an order 100 names, an account 300 orders a day. Orders and their authorizations last a
  day (an order reusing an authorization ends with it); used nonces, old orders, authorizations no
  live order lists and expired certificates' rows go in the sweep. Errors are RFC 8555
  problem documents (`urn:ietf:params:acme:error:*`) and never hold key material. State lives in the
  database: any node of a fleet answers any request; claiming a challenge or an order, using a nonce
  and issuing are each one write, and challenges are checked outside any transaction.
- The routes are outside the gate (`/v1/secrets/acme` is public in rolld's auth rules, as the CRL is):
  `directory` (GET), `new-nonce` (HEAD, GET), and POSTs to `new-account`, `acct/{id}`,
  `acct/{id}/orders`, `new-order`, `order/{id}`, `order/{id}/finalize`, `authz/{id}`, `chall/{id}`,
  `cert/{serial}`, `revoke-cert` and `key-change`.

cert-manager (inside the cluster, pre-authorized names), after `roll secrets cas acme eab add
internal --name svc.internal --label cluster-a` printed the key:

```sh
kubectl -n cert-manager create secret generic roll-internal-eab --from-literal secret=<HMAC key>
```

```yaml
apiVersion: cert-manager.io/v1
kind: ClusterIssuer
metadata:
  name: roll-internal
spec:
  acme:
    server: https://api.roll.example/v1/secrets/acme/ca_example/directory
    privateKeySecretRef:
      name: roll-internal-acme-account
    externalAccountBinding:
      keyID: eab_example
      keySecretRef:
        name: roll-internal-eab
        key: secret
    solvers:            # cert-manager needs one; pre-authorized names never use it
      - http01:
          ingress:
            ingressClassName: nginx
```

certbot and lego:

```sh
certbot certonly --server https://api.roll.example/v1/secrets/acme/ca_example/directory \
  --eab-kid eab_example --eab-hmac-key <HMAC key> --standalone -d api.svc.internal
lego --server https://api.roll.example/v1/secrets/acme/ca_example/directory --eab --kid eab_example \
  --hmac <HMAC key> --email ops@svc.internal --domains api.svc.internal --http run
```

Clients that check revocation (and TLS to the directory) trust rolld's public address as any HTTPS
client does; the certificates themselves chain to the CA's certificate (`roll secrets cas cert NAME`).

### SSH certificates

A project's own SSH CA, as Vault's SSH engine signs them: short-lived user certificates instead of
long-lived public keys on every server. Servers trust one CA key (`TrustedUserCAKeys`).

- `POST …/ssh-cas {name, principals, max_hours?, host_domains?, max_host_days?}` (admin; `roll secrets ssh-cas add NAME --principal
  deploy`, the Keys & certificates tab) makes an Ed25519 key, sealed with the project's data key and bound to
  the CA. Its public key (`ssh-ed25519 …`, `roll secrets ssh-cas key NAME`) is what servers trust.
  `principals` are the logins it signs for; `max_hours` (24 by default, a week at most) is the longest
  a certificate lives.
- The link kind `ssh` has `certificate` (the `…-cert.pub` line) and `private_key` (an unencrypted
  OpenSSH key), one credential, with the logins in `options.principals` (the Link dialog asks); a
  login the CA doesn't allow is refused when it's linked. Each lease gets a new Ed25519 key and a
  user certificate (`permit-pty`, `permit-port-forwarding`; the key id names the lease) for its TTL
  plus five minutes, at most `max_hours`. Links are `dynamic` only: a certificate lives hours, and a
  managed rotation counts in days.
- The certificate and key formats are OpenSSH's (`PROTOCOL.certkeys`, `PROTOCOL.key`), written by roll
  and checked against OpenSSH 10: `ssh-keygen -L` reads them, and `sshd` with `TrustedUserCAKeys`
  takes the certificate for its principal and nobody else's. A certificate can't be taken back, so
  lifetimes stay short; an SSH CA a key links to can't be deleted.
- **`roll ssh -e production -- deploy@host`** (as `vault ssh` and `tsh ssh`): resolves the
  environment's `SSH_CERT` and `SSH_KEY` (`--cert`, `--identity` for other keys; a dynamic link gets
  a pair for this session), writes them to a private temporary directory (0700, the key 0600), runs
  `ssh -i … -o CertificateFile=… -o IdentitiesOnly=yes` with the rest of the arguments, deletes both
  when it exits, and exits with ssh's code. A person never handles the key. Hosts the project's
  SSH CAs sign are trusted by their CA (their `@cert-authority` lines are added after the user's own
  `known_hosts`, which keeps any host the user accepts), so there's no "trust this host?" on first
  use. `--key` and `sign-user` read only a public key line: given a private key's path they read
  `<path>.pub`, and anything else is refused before it leaves the machine.
- **A key you already have** (in an agent, on a hardware token): `POST …/ssh-cas/{id}/sign-user
  {public_key, principals, ttl_minutes?}` (`reveal`, as resolving an SSH link is, since a certificate
  is a login, and `write`, since otherwise a principal comes only from a link a writer made;
  audited, and refused if the audit row can't be written; `roll secrets ssh-cas sign-user NAME ~/.ssh/id_ed25519.pub --principal
  deploy`) certifies its public key for logins the CA allows, 60 minutes by default and the CA's
  `max_hours` at most; only the public key comes to roll. `roll ssh --key ~/.ssh/id_ed25519.pub
  --ca ops --principal deploy -- deploy@host` does it per session (the private file next to it,
  or else the agent's matching key). Checked with OpenSSH 10: an RSA key held only in `ssh-agent`
  logged in to a real `sshd` as `deploy` with roll's certificate, and not as `root`.
- **Host certificates**, as Vault's SSH engine signs host keys: an SSH CA with `host_domains`
  (`--host-domain svc.internal`, a domain of the project's own: a top-level or common public
suffix such as `com`, `co.uk` or `github.io` is refused (from a built-in list of the common ones, not the whole Public Suffix List: signing hosts is an admin's, and a CA's `@cert-authority` line only reaches clients that add it); `max_host_days`, 90 by default, 397 at most) signs a server's own
  public key for its names. `POST …/ssh-cas/{id}/sign-host {public_key, hosts, days?}` (admin, since
  a host certificate lets a server stand in for those names; audited; `roll secrets ssh-cas
  sign-host NAME /etc/ssh/ssh_host_ed25519_key.pub --host db1.svc.internal`) takes the key line
  (Ed25519, ECDSA P-256/384/521, or RSA of 2048–16384 bits, parsed with bounds), checks each name is
  under a host domain (a `*.` one level under one too), and answers the `…-cert-v01@openssh.com`
  line for sshd's `HostCertificate` and the `known_hosts` line clients add: `@cert-authority
  *.svc.internal,svc.internal ssh-ed25519 …` (`roll secrets ssh-cas known-hosts NAME`, the CA's row
  in the Keys & certificates tab). The server's key never leaves it. A CA may sign users, hosts or both.
  Checked against OpenSSH 10: `ssh-keygen -L` reads all three key types' certificates as host
  certificates, and `ssh -o StrictHostKeyChecking=yes` with only the `@cert-authority` line accepts
  a real `sshd`'s Ed25519, ECDSA and RSA host certificates and refuses another name (`Host key
  verification failed`).
### Sensitive keys
A key tagged `sensitive` (`roll secrets set KEY --tag sensitive`, or its tags in the dashboard) can be
written but never read back by a person or a token, as Vercel's sensitive environment variables and
Doppler's restricted secrets: only roll apps' builds and instances get its value (`ForApps`
resolves as a runtime), where the service's code uses it. An untrusted ephemeral environment is
read without it by everyone, roll apps included: a fork's preview deployed on roll apps doesn't get
it, and leaves out dynamic keys and what doesn't resolve instead of failing the deploy.
- Everything else is a person or acts for one, so it's withheld there. The operator is no exception.
  - `…/values` and `roll run` leave it out and name it in `withheld`, so `roll run` still starts,
    saying so. `…/secrets/{key}/value` answers 403, and so does every version of a key that's
    sensitive now: tagging a key keeps its value, so an older version holds it too.
  - A reference doesn't resolve to it (`${KEY}` can't carry it into another key). A change
    request's values hide it, when it's sensitive now or the request makes it so (a promote, a new
    key). A backup carries its name, not its value.
  - Checks, sandboxes (a terminal), previews, syncs (a target its maker chose and may read back:
    the key is "not synced" and the target keeps what it had) and CI's workload identities don't
    get it.
  - Nor does another roll product's reference that a person or a token chose
    (`Secrets::bind_ref`, then `resolve_as`: search's snapshot repositories). Whoever may write
    the product's settings chooses where the value goes (their own S3 endpoint), so the key is
    refused when it's chosen, and at every read if it's tagged later. It reaches only
    operator-written storage targets (`resolve_for`, "Other roll products").
- Nothing tests a guess against it either: no fingerprint, weakness or typed-key misfit shows for
  it, comparing environments says `hidden`, a local file's diff skips it, and the leak scan
  (`POST …/leaks`) doesn't match it. roll code's own scans of pushed code still do: a sensitive
  value in code leaked like any other.
- Sensitive is for good: a write that takes the tag off is refused (delete the key and set it again
  for a value people can read).
- In-process callers say what they are (`Caller::runtime`): `Caller::consumer` is one, and
  `.not_runtime()` makes it not (checks and sandboxes). A runtime's read names the keys whose value
  holds a sensitive one, directly or through a reference, in `sensitive`, so it uses them and never
  shows them (roll apps puts them in a container's environment and masks them in its variables, as
  its own sealed ones).
### Encryption keys
Encryption as a service, as Vault's Transit engine and Infisical's KMS give: an app sends data and
gets ciphertext back under a named key that never leaves roll, so it can keep encrypted fields (card
numbers, tokens, notes) in its own database without holding a key.
- `POST …/transit {name}` (admin; `roll secrets transit add NAME`) makes a key. Each version is 32
  random bytes sealed with the project's data key, bound to the key and version, so a project's
  customer-managed key covers it too.
- `POST …/transit/{name}/encrypt {plaintext (base64), context?}` (`write`) answers `{ciphertext}`:
  `roll:v<version>:<base64url(nonce ‖ AES-256-GCM)>`. `context` (a row's id, say) is bound in: the
  same ciphertext under another context won't open. `{items: […]}` encrypts a batch (1,000 items,
  4 MiB at most; 64 KiB each) and answers `{items: [{ciphertext}]}`.
- `…/decrypt {ciphertext, context?}` (`reveal`, audited) answers `{plaintext}` (or a batch);
  `…/datakey {context?}` (`reveal`) a fresh 256-bit data key and its ciphertext, for encrypting large
  data locally (envelope encryption).
- `…/rotate` (admin) adds a version new encryptions use; older ones keep decrypting.
  `…/rewrap` (`write`) re-encrypts under the newest without the plaintext leaving roll, and
  `PATCH …/transit/{name} {min_decrypt_version}` retires older versions. `DELETE …/transit/{name}?confirm=<name>`
  deletes a key for good: what it encrypted can't be decrypted again.
- Every key also signs and computes HMACs, each with its own key derived from the version's
  bytes (HKDF-SHA256, one label per use). `…/sign {input (base64)}` (`write`) answers
  `{signature}`: `roll:v<version>:<base64url>`, Ed25519 over the input exactly as given, so a
  service holding only the public key checks it with any Ed25519 library (webhooks, tokens an app
  issues). `…/verify {input, signature}` (`read`) answers `{valid}`; `GET …/public` (`read`) is
  every version's public key as raw base64, PEM (SPKI) and a JWK (`kid` `<name>:v<version>`).
  `…/hmac {input}` (`write`) answers `{hmac}`, HMAC-SHA256, stable for one input: a lookup column
  over encrypted fields (a blind index). `…/hmac/verify {input, hmac}` answers `{valid}` and is
  `write`, not `read`: it answers guesses about the input. All take batches, as encrypt does; a
  signature or HMAC by a retired (`min_decrypt_version`) or unknown version is `valid: false`, a
  malformed one a 400. Signing is `write` on the project, as encrypting is: a grant can't yet be
  narrowed to one key, so anyone who may write the project's secrets may sign with its keys.
- A key can be brought from outside, as Vault's and Infisical's KMS imports: `POST …/transit {name,
  key, vault_version?}` or `…/rotate {key, vault_version?}` (admin; `roll secrets transit import NAME
  [--vault-version N]` with the key on stdin) takes 32 bytes (base64) as the first or newest version.
  From Vault's transit (an exportable `aes256-gcm96` key, `vault read
  transit/export/encryption-key/NAME/<n>`), name each version's Vault number: Vault's own
  ciphertexts, `vault:v<n>:…`, then decrypt under it (Vault's AAD is empty, so no context; derived
  keys aren't brought over) and `…/rewrap` moves them to `roll:v…`, so data encrypted before roll
  comes over without its plaintext leaving. Checked against Vault 1.20: its exported key brought in,
  its `vault:v1:` and `vault:v2:` ciphertexts decrypted and rewrapped. The listing marks brought
  versions (`imported`, `vault_version`); backups keep the Vault numbers.
- A failed decrypt or rewrap is audited too (probing contexts leaves a trace). A backup carries every
  key's versions, in the archive sealed to its taker's key, so a restore (here or on another server)
  still opens what they encrypted; ciphertexts are bound to their context, not to the key's id.
- Nonces are random (96-bit): rotate a key well before 2³² encryptions under one version, the bound
  AES-GCM sets for random nonces (`…/rotate`, then `…/rewrap` what matters).

## Consumption

### `roll secrets` and `roll run`

```
roll secrets ls      [-p project] [-e env] [--path /]          # keys, masked
roll secrets get     KEY [--raw]
roll secrets totp    KEY [--wait]                               # a TOTP key's current code and the seconds left
roll secrets set     KEY=VALUE… | KEY (value from stdin) [--comment] [--tag]
roll secrets rm      KEY…
roll secrets import  .env [--overwrite]                         # one change (.env or JSON)
roll secrets edit                                               # the folder in $EDITOR, one change
roll secrets set     KEY --generate 48                          # a random value made locally
roll secrets health                                            # every finding, every environment, worst first
roll secrets setup   [-p P -e ENV --path /] | --approve | --show | --clear   # this directory's project (alone: approve roll.toml's, or pick)
roll secrets doctor                                            # this machine's setup, checked
roll secrets check   [.env.example] [--strict]                # keys a template names that the environment lacks
roll secrets scan    [PATH…] | --staged [--allow-unstored]    # this project's values and credentials it doesn't hold: where, never what
roll secrets scan    --store [KEY=]FILE:LINE                   # move one such credential in; prints what to write instead
roll secrets render  config.tpl [--out config.yml [--watch --exec CMD]]   # ${KEY} filled in
roll secrets export  [--format dotenv|json|shell|yaml|docker|kubernetes] [--name N --namespace NS]
roll secrets envs    ls | add NAME [--base ENV --ephemeral] | rename OLD NEW | rm NAME | order A B C
roll secrets history [KEY]   roll secrets rollback --to SEQ | --at TIME
roll secrets diff    --from development --to staging | --file .env   # two environments, or a local copy
roll secrets promote --from development --to staging [KEY…]
roll secrets audit   [--key KEY] [--format csv|jsonl] | verify [--since TIME] [--file F --jwks F]
roll secrets syncs   ls | add KIND --target name=value… | run ID | rm ID
roll secrets sinks   ls | add URL [--from-start] [--format splunk|datadog|bearer] (token on stdin) | rm ID
roll secrets purge   KEY [--before N]        roll secrets retention 365|forever
roll secrets insights [--unused]             roll secrets shares [--revoke ID]
roll secrets leases  [--renew ID | --revoke ID]
roll secrets databases ls | add NAME [--role A --role B] [--group G] [--ca FILE] (admin URL on stdin) | rm ID
roll secrets directories ls | add NAME --url URL --bind-dn DN [--flavor F] [--account DN…] [--user-base-dn DN] [--group DN…] [--ca FILE] (bind password on stdin) | rm ID
roll secrets snowflake ls | add NAME --account A [--privatelink] --admin-user U [--admin-role R] --key FILE|- [--auth password] [--user U] [--role R…] | rm ID
roll secrets atlas   ls | add NAME --group-id ID --client-id ID [--user A --user B] [--role role@db…] (client secret on stdin) | rm ID
roll secrets aws-users ls | add NAME USER --role ROLE | rm ID
roll secrets clouds  ls | add-gcp NAME --audience A --service-account SA | add-azure NAME --tenant T --client-id C [--scope S] | rm ID
roll secrets issuer rotate [--now --retire KID]   # the operator's: the next key now; --now --retire pulls a leaked one
roll secrets kubernetes-clusters ls | add NAME API_URL [--ca FILE] (admin token on stdin) | rm ID
roll secrets cas     ls | add NAME --domain D… [--allow-ip] [--max-days N] | cert ID | sign ID [CSR|-] [--days N] [--chain]
                     | certs ID | revoke ID SERIAL | rm ID
                     | acme eab add ID [--name D…] [--days N] [--label L] | acme eab ls ID | acme eab rm ID EAB
                     | acme pre-authorize ID on|off                       (`ca` is the same command)
roll secrets ssh-cas ls | add NAME [--principal P…] [--max-hours N] [--host-domain D…] [--max-host-days N] | key ID
                     | sign-host ID [KEY.pub|-] --host H… [--days N] | known-hosts ID
                     | sign-user ID KEY.pub --principal P… [--ttl-minutes N] | rm ID
roll secrets transit ls | add NAME | rotate NAME | encrypt NAME [--context C] | decrypt NAME [--context C] | rm NAME --confirm NAME
roll secrets transit sign NAME | verify NAME SIGNATURE | hmac NAME | public NAME   # stdin is the input
roll secrets transit import NAME [--vault-version N]                                # the key on stdin
roll secrets kubernetes [--name N] [--namespace NS] [--refresh 1h] [--url URL]   # ESO manifests
roll secrets reach [KEY]                     # who and what can read it
roll run [-p project] [-e production] [--path /] [--env-file .env]… [--allow-unresolved] [--totp-codes] [--watch] [--mask | --no-mask] [--fallback FILE [--fallback-max-age 7d]] -- <command…>
roll ssh [-p project] [-e production] [--cert SSH_CERT] [--identity SSH_KEY] -- <ssh arguments…>
roll completions bash|zsh|fish|powershell
```

- The project is `-p`/`ROLL_PROJECT` (a name or id), the environment `-e`/`ROLL_ENV`, the path
  `--path`/`ROLL_SECRETS_PATH`. Without them, `roll secrets setup -p shop -e staging [--path /api]`
  remembers them for the current directory and the ones under it, in the user's own
  `~/.config/roll/secrets.json` (never the repo), so `roll run -- npm start` needs no flags there
  (`--show`, `--clear`). A project of that name that doesn't exist is made first, through the
  workspace's `POST /v1/workspace/projects` as whoever the CLI is signed in as: after a yes in a
  terminal (`Create project "shop"? [y/N]`), or with `--create` in a script. Then the repo's
  suggestion, then `development` and `/`. A scoped token (`ROLL_TOKEN`) implies its project. In a
  terminal with no `-p`, `setup` lists the projects the login can see, then the chosen one's
  environments (protected ones marked), and remembers the pick.
- **The repo's suggestion** (Doppler's `doppler.yaml`, Infisical's `.infisical.yaml`, with
  direnv's `allow`): `[secrets]` in the nearest `roll.toml` walking up from the current directory
  may hold `project`, `environment`, `path`, and `branches`, a table of branch glob to environment
  matched against the checked-out branch with roll code's glob table (`*` within a segment, `**`
  any segments, the whole name: `release/*` matches `release/1.2`; the longest matching pattern
  wins, else `environment`). It sits beside roll code's `unstored_credentials`. Any other key, and
  anything that looks like an API URL, a token or a value, is named and ignored: a repository never
  chooses where a token goes.
  - It's used only once the person has approved that exact table for that directory: `roll secrets
    setup` with no flags shows it and asks in a terminal, `roll secrets setup --approve` approves it
    in a script. The approval keeps the directory and a SHA-256 of the table as used in
    `secrets.json` under `approved` (`{"<dir>": {"sha256": …, "at": …, "table": {…}}}`, beside the
    directories' own setups), so a changed table asks again and says what it was. Until then every
    command prints what the repo suggests and how to approve it, and uses nothing from it.
  - A protected environment is never taken from it, even approved (a cloned repo can't route
    production values to the code it ships): `roll run` asks the environment's answer, which says
    whether it's `protected` (or `trusted` to read a protected base), and refuses, naming `-e`,
    `ROLL_ENV` and `roll secrets setup -e` instead. The check is made on every use, so a later
    `envs protect` applies at once, and an outage refuses rather than guesses.
  - Precedence, per value: flags, then the variables, then the person's own setup, then the
    approved table, then `development` and `/`. The table speaks for the project it names: given
    another project, none of it applies.
- `roll secrets doctor` checks that setup in one go, each line ok or FAIL: who the token is (and
  the project it's bound to), the directory's project, environment and folder and where each came
  from (a flag, a variable, your setup, `roll.toml` and the branch pattern, the default), whether
  the repo's table is approved (and what it left out), and how many keys it can
  read there, anything there that needs fixing now (expired, missing, exposed or in-the-browser keys, from health),
  the keys a `.env.example` names that it lacks, whether a local `.env` still matches (the diff
  above, by fingerprint), and whether staged files hold any of
  the project's values (the scan, names only). It fails when a check does: a first step when
  `roll run` doesn't see what it should.
- `roll run --watch [--interval 5]` keeps the command as a child and restarts it (SIGTERM, then
  SIGKILL after 10 s) when the environment changes. It polls `…/digest`, a hash of the folder's
  effective keys, versions, fingerprints and links, and of what their references reach (in this
  environment or another), that decrypts nothing, issues nothing and isn't audited
  (`Secrets::digest` in process). It compares from the digest each `…/values?digest=1` answer
  carries, which the server takes just before that read (asked for, since it costs about as much
  as the read): a write that lands while the command's values are
  being read (or re-read, or newly referenced) is still a change, never folded into what it
  compares against. The message names the folders that moved (`production:/ changed`). A change
  that doesn't resolve keeps the running command. A live link's resource rotated through roll
  moves the digest of every folder that links it (a password changed outside roll can't).
- `roll secrets edit` opens the folder's own keys as a `.env` in `$EDITOR` (a 0600 file under
  `$XDG_RUNTIME_DIR` when there is one, removed after) and applies what changed as one change
  (`import` with `replace`: a removed line deletes its key; linked and personal keys aren't in the
  file and never deleted). `set KEY --generate N` makes a random value of N letters and digits.
- `roll run` fetches the resolved values (`…/values`), adds them to the child's environment and
  execs the command. It writes no file, and it fails before running anything if a reference is
  unresolved. Variables already set in the shell keep their value unless `--override` is passed.
- What the command prints is masked in logs, as 1Password's `op run` does and GitHub Actions does
  for its own secrets: roll keeps the command as a child, reads the stream, and writes
  `<concealed by roll>` in place of any value it added, so a debug print doesn't put a key in a
  build log. By default stderr when it isn't a terminal, or in CI (`CI` set, where a PTY is a log
  too): it's diagnostics, never data. stdout is data as often as a log (`pg_dump > dump.sql`,
  `| gzip`, a dump over ssh), so it's masked only in CI and never into a file: nothing written as
  data is ever changed unasked. `--mask`, or `ROLL_MASK=1` in a pod's or a unit's environment
  where stdout is the log, masks both streams wherever they go; `--no-mask` neither. **Jenkins**
  writes a step's output to a log file, which roll can't tell from `> dump.sql`: set `ROLL_MASK=1`
  in the job's environment (`environment { ROLL_MASK = '1' }`), or stdout reaches its log unmasked
  while stderr is masked. A value is
  matched as it is, its JSON and URL encodings, each line of a multi-line one that has no spaces (a
  PEM key's lines, not a JSON file's `"type": …` lines), and, in a JSON value (a cloud key file,
  minified or not), the fields named as secrets (`private_key`, `client_secret`, not `token_uri`);
  values under 8 characters (`true`, a port) aren't masked. Output passes through as it comes, except a tail that
  could start a value, held until more comes or the stream ends (never on a timer: a
  block-buffered program can pause in the middle of a value).
- Masking is a guard against accidents, not a boundary. The command can print a value some other
  way (reversed, split, base64, Python's `ensure_ascii` JSON, `%2f`, `+` for spaces), and where
  two values overlap in the output only the first is concealed whole.
- A masked command runs as roll's child. The exit is the command's: its code, or the same SIGINT,
  SIGTERM or SIGHUP re-raised, so a shell loop stops on Ctrl-C. SIGTERM and SIGHUP (`docker stop`,
  systemd) are passed on, with or without `--watch`, and SIGINT unless roll's terminal sent it to
  the command too (so a `kill -INT` aimed at roll while it's in the terminal's foreground isn't
  passed on: send SIGTERM, or signal the command). A process the command leaves in the background gets 1 s of output after the
  command exits (then roll exits and its pipe closes): run daemons with `--no-mask`
  (`roll-cli/src/mask.rs`).
  Measured (release build, 2026-11-14, 400 MB through `cat`): with 300 values of 32 characters
  and a 3 KB key, about 330 MB/s on real logs and docs and 240 MB/s on random base64 (the
  worst case: every byte can start a value); about 1.2 GB/s with a handful of values. Unmasked,
  the same `cat` runs at about 3.5 GB/s. No command prints anywhere near that, so masking costs a
  real command nothing it notices; memory stays under 30 MB.

### Docker builds and Compose

- **A build that needs a secret** (a private registry's token): BuildKit's secret mount, from
  `roll run`'s environment. The value is there for that one `RUN` step and in no layer; a build
  argument (`--build-arg`) would stay in the image's history.

  ```dockerfile
  # syntax=docker/dockerfile:1
  RUN --mount=type=secret,id=npm,env=NPM_TOKEN npm ci
  ```

  ```sh
  roll run -e production -- docker build --secret id=npm,env=NPM_TOKEN .
  ```

- **Compose**: list the variables without values and Compose passes `roll run`'s through, so
  nothing is written to a file:

  ```yaml
  services:
    api:
      environment: [DATABASE_URL, STRIPE_KEY]
  ```

  ```sh
  roll run -e development -- docker compose up
  ```

  `docker compose config` would print them: pass `--no-interpolate` or don't share its output.
- Checked on 2026-11-14 (Docker 29.9, Compose 5.6): the build step saw the value, `docker history
  --no-trunc` and the image's `Config.Env` held none of it, a Compose service saw it, and
  `docker compose config` printed it unless `--no-interpolate`.
### Running through an outage
`roll run --fallback FILE` (or `ROLL_FALLBACK`) also writes every cleanly resolved fetch to `FILE`,
as Doppler's CLI does. When rolld can't be reached, or answers with a 5xx, `roll run` starts from
the file, saying how old it is. A refusal (a revoked token, a 403, an environment that doesn't
exist) never falls back, and neither does a file older than `--fallback-max-age` (default 7 days).
- The file is AES-256-GCM, with a key derived (HKDF-SHA256, the file's own salt, info
  `roll-fallback-v1`) from `ROLL_FALLBACK_KEY` (at least 32 characters: a copied file can be tried
  offline), else from the token. A workload identity's tokens change at every login, so it needs
  `ROLL_FALLBACK_KEY`, and a login that gets no answer goes on to the file.
- "Can't be reached" is a network error or a 5xx, from the login, the project's lookup or the
  values; a 4xx (a revoked token, a customer's key that refused: 423) or an answer that isn't
  roll's never falls back. With `--fallback`, rolld gets 5 s to connect and 30 s to answer.
- The project as asked for (`-p`), the project, environment, folder and fetch time are associated
  data: a file can't be swapped for another project's or environment's, or edited to look fresher.
- It's written 0600 through a new file renamed over the old one. Only what resolved cleanly is
  kept. A dynamic credential in it may have ended with its lease.
- With `--env-file`, the file's `roll://` references are kept too, as they resolved (under their
  own text: no variable's name has `://`). Plain values are read from the env file again; a
  reference added since the last fetch can't start from the file, and says so.
- When the project can only be found by asking rolld (a name, or the token's own), the file's own
  project is used, and checked again when it's opened. `--watch` started from a fallback reloads
  the command once rolld answers.

### The shell

- `roll completions bash|zsh|fish|powershell` prints a completion script for every `roll` command
  (clap's): `roll completions zsh > "${fpath[1]}/_roll"`,
  `roll completions bash > ~/.local/share/bash-completion/completions/roll`,
  `roll completions fish > ~/.config/fish/completions/roll.fish`.
- With [direnv](https://direnv.net), a directory's environment loads as you `cd` into it. After
  `roll secrets setup` has picked the project and environment for the directory, its `.envrc`:

  ```sh
  eval "$(roll secrets export --format shell)"   # single-quoted, so $, ` and quotes stay as they are
  watch_file ~/.config/roll/secrets.json          # a new setup reloads it
  ```

  `direnv allow` once. Every process the shell starts then sees the values, and they change only
  on `direnv reload`: for a server, `roll run [--watch]` keeps them to the one command and follows
  changes. Each load is one audited read.

### Runtime resolve for services (roll apps)

roll apps (docs/apps/) deploys a service into an environment and injects that environment's
secrets when the service starts. rolld's `ForApps` (`crates/rolld/src/secrets.rs`) is apps'
`SecretSource`: the service's own project's environment of the same name, resolved as
`app:<svc>`, only where the project stands for the service (`Workspace::trusts`). A project or
environment roll secrets doesn't have is no variables (a service needn't use secrets); an
unresolved reference or a dynamic credential fails the deploy, saying which. It uses one call:

- **In process**: `Secrets::values(project, env, path, true, Some("app:<service>"), &caller)`
  returns `Values`: a map of `Value`s (redacted in `Debug`, zeroized on drop), `problems` (fail a
  deploy on any), `dynamic` (keys holding a short-lived credential: refuse them for a service that
  scales to zero) and `linked` (keys from links, whose addresses a container network may need
  rewritten). The caller is `Caller::consumer("app:<service>")`, or `Secrets::untrusted(project,
  &caller)` for code nobody vetted. Dynamic links issue leases owned by the consumer, and the call
  is audited as `resolve` by `app:<service>`. `Secrets::digest(project, env, path, &caller)` (no
  decryption, unaudited) moves whenever the values would: compare it to decide a redeploy.
- **Preview environments are untrusted themselves.** An ephemeral environment that wasn't made
  `trusted` never reads a protected environment, whoever resolves it: an app deployed into a
  preview's environment as `app:<service>` gets its overrides and the unprotected rest, never
  production's values. Its digest leaves the protected base out the same way.
- **Over HTTP** (another node or process): `POST …/environments/{env}/resolve`
  `{"path": "/", "consumer": "app:<service>", "seal_to": "<X25519 public key>"}`. With `seal_to`
  the answer is one `roll-seal-v1` box (the same box as docs/code/agents.md, "Endpoints"; associated
  data `roll-secrets:<project>:<env>:<consumer>`), so the values are never in the clear outside the
  process that holds the private key. Without `seal_to` it is plain JSON over the internal API.

### roll search's inference endpoints

A roll search cluster's inference endpoint (the model a `semantic_text` field embeds with, a
reranker, a completion) can take its provider key from a key of the project whose canvas holds the
cluster, instead of a key pasted into it: the Inference tab's "A roll secret", or
`roll search inference create … --secret OPENAI_API_KEY` (`--environment`, or a
`roll://ENV[/folder]/KEY` reference). Rotating the key then updates every endpoint keyed by it.

- `POST /v1/search/clusters/{id}/inference/secrets` `{inference_id, task_type?, config, secret:
  {project?, environment, path?, key}}` (`config`: the `_inference` body without
  `service_settings.api_key`; `project` only when the cluster is on several canvases) reads the key
  as the caller (`reveal`, audited `secrets.reveal`; a protected environment's rules apply, a
  sensitive key is refused, and so is a dynamic credential, which ends with its lease), creates the
  endpoint with it through the cluster's own `_inference` API (which seals it, as any key), and
  makes a `search-inference` sync as the caller. The route needs `admin` on the cluster (it changes
  the cluster as the console does) and the project must stand for the cluster
  (`Workspace::trusts`). `GET` on the same route lists the endpoints that follow a secret and the
  projects whose secrets may key one.
- The sync pushes the key whenever its value changes (the sweeper; `POST …/syncs/{id}/run` pushes
  it now, the tab's refresh), as its maker. It pins where the endpoint sends the key: a SHA-256 of
  canonical JSON of its service, task type, service settings (all but `api_key`, `rate_limit`,
  `num_allocations` and `adaptive_allocations`: a URL, an Azure resource and deployment, a target,
  a host) and all of its task settings (they can carry headers, and `custom`'s request). On any
  difference the push refuses, so someone who may change the cluster but not reveal the key can't
  recreate or repoint the endpoint and wait for the next rotation; creating it again from the
  Inference tab binds it anew. One window stays: the endpoint recreated in the milliseconds
  between the push's check and its `_update`, which calls the service with the key before it
  answers. The push compares the answer's pin too, so that case fails loudly (an error log line
  and the sync's error say to rotate the key) rather than passing unseen. Its errors name
  the endpoint and Elasticsearch's error type, never the provider's message (it can quote part of
  a key).
- The secret is the endpoint's `api_key`, so it keys the services that take one (OpenAI, Azure
  OpenAI by key, Cohere, Anthropic, Google AI Studio, Mistral, Voyage AI, Jina AI, DeepSeek, Hugging
  Face). A service with other credentials (Amazon Bedrock's access keys, Vertex AI's service
  account) refuses an `api_key` when the endpoint is made (`unknown to the [service] service`), so
  no sync is made; the tab offers "A roll secret" only for the others.
- Deleting the endpoint from the tab deletes its sync; one deleted another way leaves the sync,
  whose next push says the endpoint is gone. Deleting the key leaves the endpoint its last value.
- The sync names the cluster by its id (`srch_…`, never a name: a name has no `_`) and pushes
  through roll search's control plane (`/v1/search/clusters/{id}/request`, on the cluster's owner
  on a fleet), not the cluster's endpoint, so its Basic auth user and IP allowlist don't apply. A
  deleted cluster (kept for restoring or not) is no cluster: a push fails, the sync shows the
  error and keeps its last digest, and nothing goes out. A restore, under its own name or another,
  keeps the id, so the next try (or `POST …/syncs/{id}/run`) pushes the current value to it; a
  cluster that took the old name never gets it.
  The Elasticsearch `_inference` API is unchanged: clients there send keys.

### Checks, sandboxes and previews

- **Checks**: `secrets = "development"` (or `"staging:/api"`) on a `[checks.<name>]` table in
  `roll.toml` gives the check that environment's values, under the table's own `env`. The repo must be
  on a workspace project's canvas. A protected environment never reaches a check, and neither does
  one with an unresolved reference (the check fails and says which). The consumer is
  `check:<owner/repo>#<check>`. A check's output becomes its summary, so every secret value of 6
  characters or more is replaced by `***` there. A value cut in half by the summary's length limit
  can still show in part.
- **Containers never carry values on a command line**: `docker create --env NAME` takes the value
  from the docker CLI's own environment (owner-readable only), and names docker reads itself
  (`DOCKER_*`, `PATH`, proxies…) go in a private env file. On Fargate the env travels in the run's
  bundle, as before.
- **Sandboxes**: `POST /v1/code/agents/{owner}/sandboxes` takes `secrets: {project, environment}`.
  The project must have one of the owner's repos on its canvas. Managing the code owner isn't
  enough: whoever creates the sandbox must be able to reveal the environment (or the folder asked
  for) themselves, as roll secrets answers them (`Secrets::caller_of`: their session, a roll token's
  rules, or the operator). Their rules also decide references and imports. Anyone else gets a 403
  and nothing is copied. A roll code owner token isn't anyone to roll secrets, so it can't copy
  secrets, and an agent token never manages the owner. The values are sealed with the rest of the
  sandbox's env until the backend has them. The sandbox records who copied them (`secrets_by`, the
  principal), and the read is theirs in the audit log (its actor), resolved for `sandbox:<id> for
  <principal>`.
- **Previews**: roll previews (docs/previews.md) makes an ephemeral environment per change (see
  "Ephemeral environments") and resolves it as `preview:<slug>`. A preview runs a change's code, so
  it resolves with `"untrusted": true`: an ephemeral environment based on a protected one gets its
  overrides and links, never the base's values.
- **Untrusted consumers.** Anything that runs code nobody vetted resolves untrusted
  (`Secrets::untrusted` in process, `"untrusted": true` over HTTP): no protected environment is
  read, asked for or reached through a reference, an import or a base, whatever the caller may
  otherwise do, and no dynamic credential is issued (it's left out and named in `dynamic`: a
  GitHub App token, an AWS session or a database login would act for the project until its lease
  ends). Checks and sandboxes always are; so are untrusted previews, and roll apps reading one.
- **roll tasks** runs functions in the user's own app, so it has no environment to inject. The app
  itself uses `roll run` or the resolve call.

`code_control::SecretEnv` is the one interface roll code uses, and `crates/rolld/src/secrets.rs`
(`ForCode`) implements it.

#### Leaked secrets don't land

A change that adds the value of a secret of its repo's project gets a `secret_leak` blocker, so it
can't be queued or landed: `adds the value of STRIPE_KEY (production): rotate it and remove it from
the change`. The blocker names keys and environments, never the value or the line.

- **What is checked.** The lines the change adds since its merge base (its own diff, so the check is
  O(change)). Each added line gives candidates: every run of `[A-Za-z0-9_\-+/=.:@~]`, the right-hand side
  of `KEY=VALUE` and `key: value` (quotes and a trailing `,` or `;` stripped), and the whole trimmed
  line, kept only if it looks like a credential (`credential_like`): at least 12 characters, two
  kinds of character with a digit or a capital among them, 3 bits of entropy a character, and never
  a URL without a password. Configuration kept as an environment variable (`https://shop.example`,
  `production`, `staging-database`) never blocks a landing; a low-entropy password can slip
  through. Binary files, files over 1 MiB and lines over 4 KiB are
  skipped, and at most 20,000 candidates are checked per change.
- **How it matches.** `Secrets::leaks` (`crates/roll-secrets/src/leaks.rs`) fingerprints each
  candidate with the project's data key and compares it with the stored `fp` of every secret, in
  every environment, that holds a value (plain values and managed links; live and dynamic links
  store none). Nothing is decrypted. Candidates shorter than 8 characters never match, so values
  like `true` or `8080` don't block. Every project that has the repo on its canvas is checked.
- **When.** The answer is computed before a change is queued or landed and when a single change is
  read, and cached per head with the merge base it was found from. A secret set after a head was
  checked is caught when the head or its merge base moves. A repo on no project's canvas, or a
  server without roll secrets, gets no blocker; if roll secrets can't answer, the change isn't
  blocked and the check runs again next time.
- **Values already in a repo.** A change is checked for what it adds, so a value that landed
  before (before the repo was on the canvas, or before the secret was set) is found by a scan:
  `GET /v1/code/repos/{owner}/{name}/leaks` (the repo's managers; in the Secrets surface, Access,
  "Values in code") reads the default branch's head tree with the same candidates and limits
  (text files up to 1 MiB, lines up to 4 KiB, 20,000 files and 200,000 candidates, then
  `truncated`) and answers each match as `{key, environment, folder, path, line}`, never the value. With
  `?history=N` (at most 1,000) it also reads what each of the last N first-parent commits added
  and lists the commits that added a value, `{key, environment, folder, changeset, path}`: a value removed
  since is still in the history anyone with the repo can read, so it's out and needs rotating.
- **Bounds.** A scan reads at most 256 MiB (files for the head, patches for history) and keeps at
  most 32 MiB of candidates, then says `truncated`; two scans run at once per server. It names
  keys and environments of the projects with the repo on their canvas to the repo's managers, as
  a blocker does on a change. A project's check also covers the values other projects share with
  it, each fingerprinted with its own project's key and named by that project
  (`platform/production`).
- **What it tells.** A blocker says a value is a secret's, so a writer of the repo could confirm a
  guess at a value they may not reveal by pushing it. That's accepted: they confirm a guess they
  already hold (never learn a value), only credential-like guesses count, every guess is a pushed
  change in the repo's history, and the alternative, landing a live credential, is worse. A
  project whose secrets its repo's writers must not even confirm keeps the repo off its canvas.
- **Way around.** Rotate the secret and remove the value from the change. For a false positive, an
  admin lands with `bypass` (on a queue repo, the queue bypass; on a direct-land repo, the bypass of
  review rules, which now covers `secret_leak` too), audited as `secret_leak_bypass`.

#### Credentials roll doesn't hold

The check above only knows values roll secrets already holds, so a newly pushed AWS or Stripe key
would land silently. The same pass also looks for credentials by their shape, as GitHub's push
protection and GitGuardian do, with roll's shared detectors (`roll_auth::credentials::find_all`):
AWS access keys, GitHub, Slack, Stripe live, Google, Anthropic and OpenAI keys, PEM private keys
(the whole block, which may span lines) and roll's own checksummed tokens.

- **What is checked.** Each run of lines the change adds since its merge base (O(change), with the
  limits above: no binary files, files over 1 MiB or lines over 4 KiB). A match counts when roll
  secrets doesn't hold it (it's matched by fingerprint with the candidates; one roll holds is a
  `secret_leak`), it isn't obviously a placeholder, and the repo's allow-list doesn't keep it. At
  most 200 are kept per change; the first 20 are shown and one more line counts the rest. Without
  roll secrets on the server it still runs, and nothing counts as stored.
- **What it says.** A `secret_unstored` notice on the change ("Worth knowing"), naming the kind,
  file and line, never the value: `adds what looks like an AWS access key at deploy.py:2, which roll
  secrets doesn't hold: store it as AWS_ACCESS_KEY_ID (roll secrets scan --store
  AWS_ACCESS_KEY_ID=deploy.py:2) and read it with os.environ["AWS_ACCESS_KEY_ID"], or allow it in
  .roll/secrets-allow`. It carries `path`, `line` and `fingerprint`. The key's name comes from the
  variable the value is assigned to on its line (`const stripeKey = "…"` gives `STRIPE_KEY`; a
  generic `token:` holding a GitHub token gives `GITHUB_TOKEN`), else from its kind; the read is the
  file's language's (`process.env.X`, `os.environ["X"]`, `std::env::var("X")`, `os.Getenv("X")`,
  `ENV["X"]`, `System.getenv("X")`…, `${X}` for configuration).
- **Blocking.** A repo makes it a blocker in `roll.toml` on its default branch's head (never the
  change's):

  ```toml
  [secrets]
  unstored_credentials = "block"   # default "warn": a notice
  ```

  An admin `bypass` lands over it, as over `secret_leak`, audited as `secret_unstored_bypass`.
  Blocking fails closed: while roll secrets can't say which values it holds, every credential
  counts as unstored (tried again at the next look), and a change whose diff can't be read is
  blocked with "couldn't check this change for credentials".
- **The placeholder rule** (`credentials::placeholder`). With the match's known prefix (`sk_live_`,
  `AKIA`, `ghp_`…) set aside, it's a placeholder when a part of it (split at `_` and `-`) starts or
  ends with `example`, `fake`, `dummy`, `test`, `sample`, `placeholder`, `redacted` or `changeme`
  (any case), or when it repeats one character 8 times in a row (`sk_live_xxxxxxxx…`). AWS's
  documented `AKIAIOSFODNN7EXAMPLE` never matches at all, and values like `x`, `***`, `<your key>`
  or `${KEY}` don't have a credential's shape. A private key's block is never a placeholder (its
  base64 repeats `A`): allow test keys instead.
- **The allow-list.** `.roll/secrets-allow`, read from the default branch's head for landings and
  from the working tree's top for `roll secrets scan`. One entry a line, `#` starts a comment: a
  fingerprint (`sha256:` and the first 24 hex digits of the value's SHA-256, as the notice and the
  scan show it) keeps that value anywhere; anything else is a path glob, matched with roll code's
  shared table (`*` within a segment, `**` across segments, no `/` matches the name at any depth)
  and keeping every match in those files. At most 64 KiB and 1,000 entries are read.

  ```
  # test fixtures
  tests/fixtures/**
  *.pem
  sha256:3f7c0a9e1b2d4c6e8f0a1b2c
  ```
- **The fix offered.** `roll secrets scan --store [KEY=]FILE:LINE` (below) reads the value from the
  file on the author's machine, sets it as a new key of the folder's environment and prints the
  line to write instead. roll never rewrites the change.
- **Store in roll.** On the change page each `secret_unstored` notice (or blocker) has a "Store in
  roll…" button for the repo's writers: a dialog with the suggested key (editable) and an
  environment picker, then the read to write instead with a copy button. Behind it, `POST
  /v1/code/changes/{id}/unstored/{fingerprint}/store {key?, environment?, path?}`
  (docs/code/http-api.md): roll code finds the notice on the change's current head, reads the value
  from the change's own blob at its path and line, and hands it in process to roll secrets
  (`Secrets::store_found`), which never returns it:
  - **The project** is the one the repository is trusted for (`counts_for_repo`, which asks
    `Workspace::trusts`): the operator's projects with the repo on their canvas, and a person's only
    for their own repositories. Canvas membership alone never counts, so a person who puts someone
    else's repository on their canvas gets a 422, not its credentials.
  - **The environment and folder** default to what the repo's `roll.toml` gives its checks (the first
    `[checks.*] secrets = "staging:/api"`, from the default branch's head) when the project has that
    environment, else `development` and `/`. The dialog lists the environments the caller may read.
  - **Who.** A writer on the repo, and then roll secrets asks the request's own grant, session or
    operator token for `write` in the folder, as any write: a roll token that may only read gets a 403.
  - **Only the repo's own changes.** An outsider's change (`trusted: false`, a fork's pull request)
    has no button, and the routes answer 409, whoever approved its run: its credential is whoever
    sent it, and storing it would put a value someone outside chose (their `DATABASE_URL`, their
    `sk_live_` key) into the project the repo is trusted for.
    It's one change attributed to the caller, or a change request where the environment is protected
    or a folder policy asks for approval. A key the folder already has is refused (409), never
    overwritten.
  - **What's kept.** roll secrets audits the write as every write; roll code's audit log says
    `secret_unstored_stored` with the key and where, never the value. The answer is `{key, project,
    environment, path, file, line, replacement, change, request}`.

  Until the author changes the code, the change adds a value roll secrets now holds, so it reads as
  a `secret_leak` on its next head.

#### Keys a change starts reading

The same pass over the lines a change adds also picks out the environment variables they read, by
their usual spellings (`process.env.X`, `process.env["X"]`, `import.meta.env.X`, `os.environ["X"]`,
`os.getenv("X")`, `env::var("X")`, `os.Getenv("X")`, `ENV["X"]`, `System.getenv("X")`; upper-case
names, and not the ones platforms set themselves such as `PATH`, `PORT` or `GITHUB_*`). Each one a
standard environment of the repository's project doesn't define (set in any folder, or reaching its
root through an import or its base) becomes a `secret_missing` notice on the change: "reads
SENTRY_DSN, which isn't set in every roll secrets environment". It names the key only, never the
environments, and only on a change of the repo's own (`trusted`) in a repo that isn't public: whoever
can read a change could otherwise guess names in a diff to learn what roll secrets holds. A notice
never blocks a landing, since
a variable can come from elsewhere: it's there so the key is added before the code that needs it
ships. Both carry the `key` (a `secret_leak` blocker its `environments` too, which its author could only
know by holding the value), and the change page links straight to that key in the project that has the repo
on its canvas ("Rotate STRIPE_KEY in roll secrets", "Add SENTRY_DSN in roll secrets").

#### Before a commit: `roll secrets scan`

The same check runs on a laptop, before anything is pushed: `roll secrets scan [PATH…]` walks
files (skipping `.git`, `node_modules`, `target`, binaries and files over 1 MiB) and `--staged`
reads exactly what `git commit` would record. The CLI takes each line's candidates (runs of token
characters, the trimmed line, the right-hand side of `KEY=VALUE`), and `POST …/leaks
{candidates}` (at most 20,000 a call) answers which are the project's values, by fingerprint: the
key, environment and folder, never the value. It prints `file:line KEY (environment)` and exits
non-zero when it found one, so it works as a hook:

```
# .git/hooks/pre-commit
exec roll secrets scan --staged
```

A scan only names values its caller may reveal, and among personal values only the caller's own:
otherwise it would confirm guesses at values the caller can't read. Each scan is audited (`secrets.scan`, the keys it found).

It also lists credentials the project doesn't hold ("Credentials roll doesn't hold" above: the same
detectors, placeholder rule and `.roll/secrets-allow`, read from the working tree's top), as
`file:line  an AWS access key, not in roll secrets: roll secrets scan --store AWS_KEY=deploy.ts:2
[sha256:…]` (`--json`: `{file, line, unstored: true, kind, fingerprint, suggested_key}`). A plain
scan only reports them; `--staged` refuses the commit unless `--allow-unstored`. To move one in:

```
$ roll secrets scan --store deploy.ts:2            # or --store AWS_KEY=deploy.ts:2 to name it
Stored an AWS access key from deploy.ts:2 as AWS_KEY in production:/
Replace it with process.env.AWS_KEY (roll run puts AWS_KEY in the environment), and rotate it if it was ever pushed
```

The value is read from the file, never typed or pasted, and goes in one change to the folder of
`-e`/`roll secrets setup`; a key that's already there is refused, never overwritten.

### A local copy against the environment

`roll secrets diff --file .env` (`POST …/environments/{env}/diff`) compares a local `.env` or JSON
copy with the folder: the keys the file adds, the ones whose value differs, the same ones, and the
ones it lacks, by fingerprint on the server, never printing a value; it fails when they differ, so
a stale copy shows before it misleads. Values with references and links' values can't be compared
with a copy and are listed apart. Comparing is a way to test a guess, so it needs `reveal` and is
audited (`secrets.compare`).

### Moving from another manager

`roll secrets import` takes a `.env` file or JSON: an object of `KEY: value` (numbers and booleans
as their text) or an array of `{key, value, comment?}`. Export from the old manager, then import
into each environment:

Or straight from its API, with your own token in its usual variable, read on your machine (never
through roll's server) and imported as one change:

```
DOPPLER_TOKEN=dp.st…  roll secrets import --from doppler --source shop/prd -e production
VAULT_ADDR=https://vault.example VAULT_TOKEN=hvs…  roll secrets import --from vault --source secret/shop -e production
INFISICAL_TOKEN=…  roll secrets import --from infisical --source <workspace id>/prod/api -e production
VAULT_ADDR=… VAULT_TOKEN=…  roll secrets import --from vault --source secret/shop --recursive -e production
roll secrets import --from aws --source prod/shop/ -e production      # your aws CLI's profile or SSO
```

- `--recursive` (Vault) walks a KV v2 tree: each secret goes into its own folder under `--path`
  (`secret/shop/db` → `/db`, the secret at the source itself → `--path`), one change per folder;
  a Vault name that can't be a roll folder (letters, digits, `_`, `-`) stops it, naming it, and
  so do more than 500 secrets, 8 levels or 2,000 paths (import in parts). A secret the token can't
  read stops it too, naming it (only a 404 is "nothing there"), so nothing is left out unsaid;
  secrets are read and folders listed only as Vault lists them, so a token with read on `app/*`
  but not on `app` itself works. Answers are read up to 4 MiB.
- `--from aws` reads Secrets Manager through the user's own `aws` CLI (its profiles, SSO and
  roles; `ROLL_AWS_CLI` names another binary): every secret whose name starts with `--source`
  (500 at most). A JSON object's fields come in as keys; any other string as one key named after
  the secret's last segment (`prod/shop/sendgrid-key` → `SENDGRID_KEY`); binary secrets are left
  out, named; JSON `null` fields too. Two secrets that give one key stop it, naming both (import
  them into different folders), and so do more than 500 under the prefix. Values come back on the
  CLI's output, never on a command line.
- Checked against Vault 1.20 (a three-level tree into three folders) and Secrets Manager on
  LocalStack (a JSON secret, a plain one, and one outside the prefix left alone).

Through the manager's own CLI, signed in as you already are there (its profiles, SSO or service
account token), never through roll's server:

```
roll secrets import --from 1password --source Prod -e production             # op: every item of the vault
roll secrets import --from 1password --source "Prod/Main DB" -e production   # one item's fields
roll secrets import --from bitwarden --source <project id> -e production     # bws (BWS_ACCESS_TOKEN)
roll secrets import --from aws-parameter-store --source /prod/shop -e production --sensitive
roll secrets import --from gcp --source my-project/prod- -e production       # gcloud
roll secrets import --from azure --source shop-prod-kv -e production         # az
roll secrets import --from akeyless --source /prod/shop -e production
```

| `--from` | Reads | Keys |
| --- | --- | --- |
| `1password` | `op item list --vault V`, then `op item get ID --reveal` per item | `VAULT`: `ITEM_FIELD` (`Main DB` + `password` → `MAIN_DB_PASSWORD`; a named section adds itself, `MAIN_DB_REPLICA_PORT`). `VAULT/ITEM`: `FIELD`. Words are uppercased, runs of anything else one `_`. Every field with a value comes in, since `op run` and `op inject` reference any field (`op://Prod/Main DB/username`), except notes and one-time passwords; concealed fields are the secret ones. |
| `bitwarden` | `bws secret list PROJECT_ID` | Bitwarden's own keys, as they are; one that isn't a roll key stops it, naming the secret. Two secrets with one key (Bitwarden allows it) stop it. |
| `aws-parameter-store` | `aws ssm get-parameters-by-path --path P --recursive --with-decryption` | The path under `--source` is the folder under `--path` and the last segment the key (`/prod/shop/db/password` → `/db`, `PASSWORD`), one change per folder, as Vault's `--recursive`. A folder segment that can't be a roll folder stops it. SecureStrings are the secret ones. |
| `gcp` | `gcloud secrets list --project P`, then `gcloud secrets versions access latest` per secret | Each secret whose id starts with the prefix, named after the rest of its id (`prod-db-url` under `prod-` → `DB_URL`). One with no version is left out, named; so is a binary one. |
| `azure` | `az keyvault secret list --vault-name V`, then `az keyvault secret show` per secret | `db-url` → `DB_URL`: the azure-key-vault sync's `_` → `-` in reverse (Key Vault's names are case-insensitive, so two can't give one key). Disabled secrets and the ones backing certificates are left out, named. |
| `akeyless` | `akeyless list-items --path P --type static-secret`, then `akeyless get-secret-value` per item | As Parameter Store: folders from the path, the last segment the key (`/prod/shop/api-key` → `API_KEY`). |

- Unless a row says otherwise, a name becomes a key uppercased, any other character `_`, a leading
  digit after a `_`, as `--from aws` does.
- Every one keeps the guarantees above: at most 500 secrets (more stops it: import a narrower
  `--source`); each CLI's answer read up to 16 MiB and killed after 60 s; a secret the CLI can't
  read stops it, naming it and the CLI's error (only the manager's "not found" is nothing there);
  two secrets that give one key stop it, naming both. Everything is read before anything is
  written, so a stopped import writes nothing.
- Names go on the CLI's command line, never values: values come back on its output only, and no
  error or line `roll` prints holds one.
- `--dry-run` (any `--from`) prints the folders and keys it would import, never a value, and
  imports nothing. Then the import is one change per folder, refused or opened as a change request
  where the folder is protected, as any other import.
- `--sensitive` (any `--from`) tags what the source keeps secret `sensitive`: Parameter Store's
  SecureStrings and 1Password's concealed fields, and every value of the other managers. It's
  opt-in because sensitive is for good: no person or token reads the key back, `roll run` leaves it
  out, and the tag never comes off ("Sensitive keys").
- Checked against each vendor's documented output only (fakes in
  `crates/roll-cli/tests/secrets.rs` print those shapes), not against real accounts.

`--map dev=development,stg=staging,prd=production` does a whole project: each `<source>/<name>` (a
Doppler config, a Vault path, an Infisical environment) into its roll environment, one change
each, reported per environment. The dashboard's import dialog shows these commands, since a browser
can't call the other manager's API. Doppler's own `DOPPLER_*` keys are left out; numbers and booleans come in as their text. Hosts
come from the managers' own settings (`DOPPLER_API_HOST`, `VAULT_ADDR`, `INFISICAL_API_URL`).

```
infisical export --env=prod --format=json > prod.json   &&  roll secrets import prod.json -e production
doppler secrets download --no-file --format json > prod.json  &&  roll secrets import prod.json -e production
vercel env pull .env.production --environment=production  &&  roll secrets import .env.production -e production
```

### VS Code

`apps/vscode` is roll secrets for VS Code (as Doppler's, Infisical's and 1Password's extensions):

- **Where the code reads a variable** (`process.env.X`, `import.meta.env.X`, `os.environ["X"]`,
  `os.getenv`, `os.Getenv`, `env::var`, `ENV["X"]`, `getenv`, `System.getenv`,
  `Environment.GetEnvironmentVariable`), a hover says which environments set it in the folder, and
  which this token can read don't. A warning marks a key roll sets somewhere but not in the
  workspace's environment (`STRIPE_KEY isn't set in staging (set in development)`); names roll
  doesn't know at all (`PATH`, `NODE_ENV`) are left alone. Names are completed where a name goes.
- **In a `.env` file**, a line with a plain value offers "Replace with `roll://ENV/KEY`" when the
  environment has the key, and "Move to roll": after a confirmation that names the project,
  environment and folder (and says when it replaces roll's value), the value is set in roll as one
  change and the entry becomes its reference, which `roll run --env-file` resolves. The file is
  read as the CLI reads it (a double-quoted value over several lines, a comment after its quote,
  the last entry of a key set twice; nothing is offered on a file the CLI would refuse), and the
  whole entry is replaced, every line of a multi-line key; if the entry changed while roll
  answered, it's left alone and the message says so. A protected environment's 409 says to open a
  change request in the dashboard.
- **What it reads**: the folder's grid (`GET …/grid`: names and which environments set them, never
  a value; `read` is enough), cached a minute. It writes only for "Move to roll". A value never
  goes into a message, a log or the extension's state.
- **Setup**: what `roll secrets setup` remembers for the folder (`~/.config/roll/secrets.json`),
  or the `roll.project`, `roll.environment` and `roll.path` settings. All of them, and
  `roll.apiUrl`, come from your own settings only, never a repository's `.vscode/settings.json`
  (which could send your token elsewhere, or point "Move to roll" at another project or
  environment), and a confirmation names the project it writes to. The status bar's
  `roll: shop · staging` chooses another environment for the workspace. The token is a project
  token kept in VS Code's secret storage (`roll secrets: Sign in`), or `ROLL_TOKEN`.
- Checked in code-server 4.96 (VS Code 1.96) against a live rolld: the hover, the warning, and a
  `.env` value moved into staging and replaced by `roll://staging/STRIPE_KEY`.

### Backup and restore

`roll secrets keygen` makes an X25519 key pair (the secret key in a 0600 file, the public key
printed); `roll secrets backup --to <public key> --out shop.backup` writes the whole project:
every environment (with its base, labels, expiry, trust, protection, approvals and age limit), its
folders, its own keys with their values, comments, tags and links, its imports and the project's
required keys. The archive is sealed to that key (`roll-seal-v1`, associated data
`roll-secrets-backup:<project>`), so the file is safe at rest anywhere and only the key's holder
opens it; `POST …/backup {recipient}` (admin, audited `project.backup`) is the call. Linked keys
carry their link and no value: a restore issues its own credentials. Personal values, history,
leases, tokens and identities aren't in it.
So that a backup restores without the one person who holds its secret key, split that key into
shares (`roll secrets key split --file roll-secrets-backup.key`; see "Splitting the deployment
key").

Environments → Backup downloads the same file from the dashboard, sealed to a public key pasted
there: the browser never holds the secret key.

`roll secrets restore shop.backup --key roll-secrets-backup.key` opens it on the CLI's machine and
loads it into the selected project, which must have no secrets yet (`POST …/restore` with the
opened archive; admin, audited `project.restore`), on this server or another. Values are written
before any environment is protected, so a restore isn't a change request; what can't be restored
(a link to a resource that project doesn't have, an import of another project's folder) comes back
as a warning, by name. Disaster recovery, and moving a project between roll servers.

**Scheduled backups** (Vault's automated snapshots, Akeyless' DR): `PUT …/backup/schedule
{bucket, recipient, every_hours?, keep?}` (admin; default every 24 h, keeping 30, at most 500) writes the same
sealed file (`{sealed, context}`, as `POST …/backup` answers) into one of the project's own roll
buckets, as `roll-secrets/backups/<project>/<time>.json`, the first one at once. Each is read back
and compared before it counts, and the oldest beyond `keep` are deleted (only roll's own
`<time>.json` files there: nothing else in the bucket is touched). roll never holds the key
that opens them: the recipient's secret key restores one as above. One node runs each (a schedule
is claimed by moving its next time on); what a person set up runs only while they can still reach
the project, and only into a bucket the project still has. `GET …/backup/schedule` says when the
last one landed, its key, and why the last attempt failed; `DELETE` stops it (what's in the bucket
stays).

### Checking against a template

`roll secrets check .env.example` compares the keys a `.env`-style template names (`KEY=…`,
`export KEY=…`, a bare `KEY`; not comments) with the keys the environment has, its imports and base
included, by name only: it reads no value, so `read` is enough. It lists each `missing` key and each
`extra` one, and fails when one is missing (with `--strict`, also when there's an extra), so a CI
step catches a variable the code started reading before an environment has it.

### References in files

As 1Password's `op://`: a file committed to the repository holds `roll://` references where values
would go, so it carries the shape of the configuration and no value.

- `roll://ENV/KEY` is a key in this project's environment, `roll://ENV/folder/sub/KEY` one in a
  folder, `roll://PROJECT@ENV/KEY` one in another project (its name, or its `prj_…` id with a
  scoped token; a name that more than one project has, under different owners, is refused: give
  the id). Segments are letters, digits, `-`, `_` and `.` (never `.` or `..`).
- In the dashboard, a key's menu has **Copy reference**: the reference to the folder it's shown
  in, so it resolves to what that folder reads (its own value, an import, an inherited one). On a
  page that isn't https the browser has no clipboard, and the note shows the reference to copy.
  (The desktop app's deep links also start `roll://`, as `roll://agents/thread/…`; the two never
  meet: references are read from files and templates, deep links from the OS.)
- `roll run --env-file .env` (repeatable) adds a dotenv file's entries to the environment's:
  plain values as written, references resolved, and the file's entries win over the environment's
  (the shell's own variables still win over both unless `--override`). A file such as
  `DB_URL=roll://production/DATABASE_URL` and `PORT=8080` runs with production's value and 8080.
- `roll secrets render` resolves references in a template where they stand (a reference runs to
  the first space, quote, bracket or comma) and fills `${KEY}` around them; neither kind of value is
  read again as a template.
- Each environment and folder is one read through the same audited `values` route, under the
  caller's own access: a reference to an environment the token can't read fails, naming it, and so
  does one to a key that isn't set or doesn't resolve. Nothing new runs on the server.
- `roll run --watch` and `roll secrets render --watch` follow every environment a file references
  too (their digests), and renew each dynamic credential's lease at its own project.
- A dotenv file is one `KEY=value` per line (quoted values on one line; a quote that runs past its
  line is refused), and its errors name the line, never its text.

### Other roll products

A roll product that needs an outside credential takes a `roll://ENV[/folder]/KEY` reference
where it would otherwise keep its own sealed copy, so the value lives once, in roll secrets, and
a rotation reaches the product without anyone pasting it again (as 1Password's `op://` and Vault
Agent's templates do).

- **Who chose it** decides how a product reads it:
  - **A person or a token** (roll search's snapshot repositories): the product calls
    `Secrets::bind_ref(resource, reference, caller)` when the reference is written, with the
    request's caller, and stores the opaque `Maker` it returns (it serializes as a string) beside
    the reference. Every read is `Secrets::resolve_as(resource, reference, &maker)` or
    `digest_as`: the reference reads *as its maker*, never as a runtime, through the same code a
    reveal runs. So a sensitive key is refused (when it's chosen, and at every read if it's tagged
    later), an import, a base or a `${…}` inside the value counts only where the maker may reveal,
    and folders a token's rules leave out stay out. The maker's rights are looked up on every
    read, never cached: a person while they can sign in and still own the project (an admin while
    they still are one), a token while it lives. An admin's session is kept as the person
    (`usr_…`), not `operator`. A workload identity's login can't choose one (400): it ends with its
    token. The audit row is still the resource's, naming the maker
    (`resolved KEY for search:search:logs, for usr_… (who chose it)`).
  - **A deployer, for the resource's own runtime** (roll apps' git login: a person pastes a deploy
    token, tagged sensitive, and the service's login refers to it):
    `Secrets::bind_ref_runtime(resource, reference, caller, runtime_env)`. The contract is the
    product's: it calls this only after its own check that the caller holds deploy on the
    resource in `runtime_env` (for roll apps, naming the login is already `deploy`); anything else
    calls `bind_ref`. Whoever may deploy there can already run code that reads that environment's
    sensitive keys, so this adds no reach:
    - A reference to exactly `runtime_env` is a *runtime binding*. Its `Maker` serializes as
      `usr_…@runtime:production` (a plain maker's format is unchanged). It reads that one
      environment as the resource's runtime: sensitive keys included, imports and bases as the
      runtime follows them, within the resource's trust (its one trusted project, and an untrusted
      copy still reads as an untrusted caller, so no protected environment and no sensitive key).
    - Every `resolve_as` and `digest_as` still checks first that the maker can sign in, still
      reaches the project and may reveal the folder, and refuses otherwise.
    - The audit row is `<resource> (runtime read for <maker>)`, e.g.
      `apps:service:web (runtime read for usr_…)`.
    - A reference to any other environment binds by the maker's rules above, and a runtime
      binding refuses any environment but its own. A workload identity's login is refused here
      too.
  - **The operator** (roll storage's replication targets, which only the operator writes):
    `Secrets::resolve_for` / `digest_for` read as the project's runtime, sensitive keys included.
    These and runtime bindings are the only references a sensitive key reaches.
- **Which products.** roll storage's replication targets to buckets outside roll: the access key
  id and the secret (`POST /v1/storage/replication-targets` with
  `"secret_access_key": "roll://prj_…@production/AWS_SECRET_ACCESS_KEY"`). A target's references
  name their project by its `prj_…` id: the bucket replicating only proves which projects stand
  for it, so without one, anyone who may set another bucket's replication could point it at the
  target and have roll sign for the target's endpoint with that bucket's project's credential. Search snapshot
  repositories, roll code's reviewer provider keys and the agents' gateway upstreams still keep
  sealed copies. roll search's snapshot repositories take them through `bind_ref` (#361).
- **Who reads it.** The product asks rolld in process (`resolve_as` or `resolve_for`), naming the workspace resource that needs the value: a replication target
  resolves for the source bucket that replicates through it. The project is the one whose canvas
  holds that resource *and* that stands for it (`Workspace::trusts`): a person's canvas only
  for what's theirs, never membership alone. A reference names a project
  (`roll://PROJECT@ENV/KEY`) only if it's that one, and several such projects need it named.
  Why one doesn't resolve stays in rolld's log: storage tells an S3 client setting a bucket's
  replication only that the target's credentials don't resolve, never "protected" or "no such
  key", which would say which keys exist where.
- **Protected environments.** As a roll apps service reads its own environment: a resource its
  project stands for reads as that project's runtime (or, through `resolve_as`, as its maker),
  protected environments included (sensitive keys only through `resolve_for`). An ephemeral environment that isn't trusted never reaches a protected one (its base,
  an import or a reference), and what only comes from there is refused as protected. A dynamic
  credential is refused: a product keeps its connection past any lease.
- **Untrusted copies.** A product passes `ResourceRef::untrusted()` for a resource that keeps
  another's id but whose code or data didn't come through that resource's owner: a branch (roll
  search's branches keep their cluster's id), a copy or a clone, anything like a preview. It then
  reads as checks, sandboxes and previews do (`Secrets::untrusted`, not a runtime): a protected
  environment, or a key only one holds, is refused as protected, and a sensitive key as can't be
  used. The hint only narrows; nothing widens a resource past its project's runtime. A restore of
  the resource itself (its own snapshot or backup, back into it) stays trusted.
- **Audit.** Every read is `secrets.resolve` by the resource, named `<product>:<kind>:<ref>`
  (`storage:bucket:photos`, `search:search:logs (untrusted)` for an untrusted copy), with the one
  key read.
- **Refusals** say which part failed, never the value: no project, not trusted, not allowed (the
  maker may not reveal it, or can't reach roll secrets any more), no such key, protected, or can't
  be used (a dynamic credential, a sensitive key for anyone but a trusted runtime, a reference
  inside the value that doesn't resolve). `digest_for` and `digest_as` refuse exactly what their
  resolve would; `bind_ref` refuses what `digest_as` would for the caller (it decrypts nothing, so
  a `${…}` reaching a sensitive key fails at the first read).
- **Rotation.** Nothing is cached across calls in roll secrets. A product keeps what it built
  (storage: one client per target and source bucket) while the folder's digest
  (`Secrets::digest_for`, no value, not audited) stays the same, and resolves again when it
  moves, so the next connection after a change in roll uses the new value. A reference that stops
  resolving fails visibly where the product works (storage: replication waits and retries, with
  why in its log), and nothing is dropped.
- **Who may write one.** Storage takes references in a replication target only from the operator
  (its token, an admin's session): a scoped token that manages targets could otherwise point a
  reference at any key the bucket's project holds and send it to an endpoint it chose.

### Templates

For a tool that reads a config file rather than its environment, `roll secrets render
config.tpl --out config.yml` fills each `${KEY}` from the environment (resolved, so references,
imports and links count) and writes the file 0600; without `--out` it prints it. A key with no
value fails the whole render, naming every such key, so a half-filled file is never written.
`\${KEY}` stays a literal `${KEY}`, and anything else (`$KEY`, `${KEY:-default}`) is left as
written, so shell snippets in the file survive. The values are read through the same audited
`values` route `roll run` uses; nothing new runs on the server.

With `--watch` it keeps running, like `roll run --watch`: it checks the environment's digest every
`--interval` seconds (default 5), renders again when it changes, and replaces the file whole (a
file beside it, then a rename, so a reader never sees half of one) only when the text changed;
then `--exec "systemctl reload nginx"` runs through `sh -c`. A render that fails (a key deleted)
leaves the last good file in place and says why. Dynamic credentials' leases are renewed every 10
minutes.

### Finding a key

`roll secrets find STRIPE_KEY` (or `GET /v1/secrets/search?key=`) lists every project, environment
and folder that holds a key, among those the caller may read (a token, its own project's), with its
version, when it changed and whether it's linked: never its value. `STRIPE_*` finds every key that
starts with `STRIPE_`. For rotating an organisation-wide key: where is it, so nothing is missed.
In the dashboard, "Set everywhere…" under an exact key's places then gives each place one new value:
one change per environment, a change request where it's protected, and a linked place rotated by
its provider instead; each place's outcome is listed, so a refusal in one doesn't hide the rest.

### Sharing a value once

`roll secrets share STRIPE_KEY [--views 1] [--ttl 24h] [--note …]` (or a value on stdin) prints a
link that opens once, or `--views` times, before it expires (5 minutes to 7 days), for handing a
credential to someone outside roll. `roll secrets open <link>` and the dashboard's share page
open it.

- With `ROLL_WEB_URL` (the dashboard's address) set on rolld, `POST …/shares` answers `link`
  (`<dashboard>/share/<id>`), and `roll secrets share` prints `link#key`.
- **The server never sees the value.** The client encrypts it with a random 256-bit key
  (AES-256-GCM, `roll-share-v1` as associated data) and puts the key in the link's `#fragment`,
  which browsers never send. The server keeps the ciphertext, and drops it at the last open, at
  expiry or when it's revoked.
- **Opening is a `POST`** (`/v1/secrets/shares/{id}/open`), so a chat's link preview, which only
  `GET`s, can't use it up. `GET /v1/secrets/shares/{id}` shows the note and the opens left. Both
  routes are public; the id is 120 random bits.
- Creating, opening (with the opener's address) and revoking are in the project's audit log.
  `GET …/shares` lists a project's shares (never their ciphertext); `DELETE …/shares/{id}` revokes
  one. A project has at most 200 open at once.
- **Where it came from.** A share of a stored key (the CLI's `share KEY`, a key's "Share once" in
  the dashboard) says so: `from: {environment, path, key}`, checked against the caller's `reveal`
  there. The server can't see that the ciphertext holds that value; it records the claim so an
  incident's cut-off of that key or folder expires the share ([Reach](#reach)). A share without
  `from` (a value typed or piped in) is counted by reach as `unattributed_shares`.

### Asking for a value

The other way round, as Infisical's secret requests: a link that asks someone outside roll (a
vendor, a contractor) to send a value in. `roll secrets request VENDOR_KEY [VENDOR_SECRET…]
[--note …] [--expires 7d] [--answers 1]` prints the link (only `/request/<token>` when neither
rolld nor the CLI knows the dashboard's address: open that path on the dashboard); in the dashboard, a folder's **Request a
value…** makes one, and Access → Secret requests lists them with **Cancel**. `roll secrets
request` alone lists them, `roll secrets request --cancel ID` cancels one.

- **Asking is a `write`** on the folder (`POST …/environments/{env}/value-requests?path=`): the
  answer is written as whoever asked, as they can write when it arrives (a person who can't sign
  in any more, or a revoked token, and the link is a 410). 1–20 key names, a note (shown on the
  page), an expiry from 5 minutes to 30 days (7 days by default), and how many times it may be
  answered (1 to 10, once by default). A project has at most 200 open at once.
- **The link's token is the capability, not the id.** Asking makes an answer token (`rlq_` and 40
  characters, the last 6 a checksum, as roll's other secrets): `create` answers it once (`token`,
  and `link` as `<dashboard>/request/<token>` when rolld has `ROLL_WEB_URL`; the dashboard makes the
  link from its own address otherwise). roll keeps only its SHA-256; the list and the record never
  carry it, so reading a project's requests (`read`) can't answer one. The id (`vrq_…`) stays for
  listing and cancelling, and is what the boxes are bound to. The token is a roll credential to
  the MCP guard and leak scanning: one written where others read it is refused, and one found in a
  landing or reported by GitHub's secret scanning cancels its request at once (audited as
  `value_request.cancel` by `leak`).
- **The page** (`<dashboard>/request/<token>`) shows only the note and the key names: never the project, the environment or who asked. Whoever has the link
  pastes the values and sends them; the page shows nothing back.
- **Sealed from their browser.** Each request has its own X25519 key pair, made by roll. The page
  seals each value to its public key (`roll-seal-v1`, as backups and endpoint keys are, with
  `roll-secret-request:<id>:<KEY>` as associated data, so a box only opens as that key of that
  request); the secret key is kept sealed with the project's data key, as every value is, and is
  dropped when the request closes. roll opens the boxes only inside the write that lands them, so
  the value is in the clear nowhere between the answerer's browser and its encrypted version: not
  in a proxy, a request log or the dashboard's server. Unlike a share's, the value is meant for
  roll, which must be able to read it in the end; a key only the asker's browser held would only
  add a step (they'd open it and write it themselves).
- **It lands** as one change on the folder, by `request:<id>` with "asked by …" in its message,
  or, where the folder needs approval (a protected environment, a folder policy), as a change
  request by whoever asked: an approver other than them approves it. The answer is claimed in the
  same transaction, so an answer that doesn't land (a value that doesn't fit its typed key, a box
  that doesn't open, a missing key) uses nothing up, and a request answered once never lands twice.
- **Stored as sent.** An answer is never a reference: a `${` in it is kept with the references'
  escape (`$${`), so `${production.DB_PASSWORD}` sent for `VENDOR_KEY` resolves to that text,
  never to production's password, and a vendor's value that happens to hold `${` arrives intact.
- **No replays.** A request keeps a digest of every box it has landed and refuses an answer that
  repeats one (409). The digest is of the box's decoded ephemeral key alone: each box has its own,
  bound into its key, so an answer sealed afresh never repeats one, boxes seen on the way can't be
  sent again to use up the answers left, and changing anything around the key (an encoding, the
  version) doesn't make a replay look new. Only `"v": 1` boxes open, everywhere roll opens one.
- **Audited** when it's asked for (`value_request.create`), answered (`value_request.answer`, with
  the answerer's address) and cancelled (`value_request.cancel`); the change or change request is
  audited as any other. The environment's notify URL hears that it landed (key names and the
  change, never a value); a change request's own notice goes to its approvers.
- **The public routes** are `GET /v1/secrets/value-requests/{token}` (`{id, note, keys,
  expires_at, public_key}`) and `POST /v1/secrets/value-requests/{token}/answer` (`{values: {KEY:
  box}}`, 204). An unknown token, a request's id, or text whose checksum fails is the same 404
  naming nothing; one answered as often as allowed, expired or cancelled is a 410 `gone`.
  Answers are capped at 5 a minute per request (counted in the request's row, so every node
  shares it and attempts at other requests never reset it) and 10 per client address (429), and a
  value at 64 KiB, as a share's.
- **MCP**: there's no tool for it. The link is for a person outside roll, and an agent that may
  write a folder can already set its values; `GET …/value-requests` is readable through the API
  with `read`.

## Syncs

A **sync** pushes an environment's resolved values to a platform outside roll, one way, whenever
they change: GitHub Actions secrets, GitLab CI/CD variables, Azure DevOps variable groups, Vercel, Netlify, Render, Fly.io,
Railway, Heroku, DigitalOcean App Platform, CircleCI, Bitbucket Pipelines, Terraform Cloud, Cloudflare Workers, AWS Parameter Store and Secrets Manager, Google Secret Manager, Azure Key Vault, a Kubernetes Secret, an endpoint
of the team's own, or a deploy hook that only hears that something changed; also Cloudflare Pages,
Supabase Edge Functions, Azure App Configuration and a HashiCorp Vault (or OpenBao) KV secret.
Infisical's "secret syncs" are the reference. roll secrets stays the source of truth: a sync never reads from its
target, and a value edited there is overwritten at the next
change of that key. Before one is added, `POST …/syncs/preview` (`roll secrets syncs add … --preview`, the form's
Preview) says what it would push, names only, without calling the platform: its names after `keys`
and `prefix`, the credential keys it reads (never pushed), and any credential or listed key the
folder doesn't have.

roll-secrets knows no platform. Each kind is a `SyncTarget` (`crates/roll-secrets/src/syncs.rs`)
that rolld registers (`crates/rolld/src/secrets/syncs.rs`), as link providers are:

```rust
pub trait SyncTarget: Send + Sync {
    fn kind(&self) -> &str;                                   // "github", "webhook", "notify"
    fn label(&self) -> &str;
    fn fields(&self) -> &[SyncField];                         // what the dashboard asks for
    fn values(&self) -> bool { true }                         // false: pushes carry key names only
    fn signed(&self) -> bool { false }                        // a signing secret, shown once
    fn credentials(&self, target: &Json) -> Vec<String> { vec![] } // keys holding its token
    // On create and before every push; `resources`: the project's canvas, as (kind, ref).
    fn check(&self, project: &str, resources: &Resources, target: &Json) -> Result<()>;
    // A name the platform keeps for itself (Supabase's `SUPABASE_`): refused on create and preview.
    fn refuses(&self, name: &str) -> Option<String> { None }
    // `push`: the values by target name, which names changed, which were removed, the digest,
    // the signing secret, the resolved credentials.
    fn push(&self, target: &Json, push: &Push) -> Result<()>;
}
```

A sync is `{id: "syn_…", environment, path, kind, target, keys, prefix, status, last_digest,
last_synced_at, error}`:

- **What it pushes.** The folder's effective values (own, imported, inherited from a base),
  resolved, as `roll run` gets them. `keys` limits it to some keys (default: all); `prefix` is put
  in front of every name at the target (`prefix: "APP_"` pushes `DB_URL` as `APP_DB_URL`). Personal
  values never sync, and neither do dynamic links (a short-lived credential would expire at the
  target): a dynamic key, or a reference that can't resolve, is left out and named in `error`.
- **When.** A change marks its project's syncs due in the same transaction (as does roll rotating
  a live link's resource), and every node looks for due syncs every 2 seconds (one indexed read
  when there are none), so a change reaches its targets in seconds however many syncs there are,
  as Doppler's and Infisical's do. The sweeper (every node, every 30 s) still goes through the
  rest, least recently checked first, each node claiming a batch of its own. Either compares the
  sync's environment `digest` (the hash `roll run --watch` polls) with the sync's
  `last_digest`. A sync whose digest moved is claimed with a compare-and-swap on its
  `running_until` (5 minutes), so two nodes never push one sync at once. A new sync first pushes
  within seconds. `POST …/syncs/{id}/run` pushes now, every key, whatever the digest says. The
  2-second pass takes only the syncs a change marked; a credential changed where roll doesn't see
  it (SQL, a product's own automation) reaches syncs at the next 30 s sweep, through the digest.
- **Only what changed.** A sync remembers a stamp per name it pushed (the secret's id, version,
  fingerprint and link: no value). A push carries the values of every synced key and says which
  names `changed` since the last push, and which were `removed`: synced before, gone now (deleted,
  or left out by `keys`). Targets that set one value per call (GitHub) send only the changed ones
  and delete the removed ones at the target.
- **As whom.** A sync resolves with the access of whoever created it (`created_by`), looked up at
  every push: a person while they can sign in and still reach the project (an admin's session is
  kept as the person, `usr_…`, so a demoted or disabled admin's syncs stop), the scoped token that
  made it (a revoked or expired token stops the sync with an error), or the operator for the
  deployment's own token. A workload identity's login can't make one (400). Syncs an admin's
  session made before this are `operator` and stay so: health flags them (`operator_made`). So a reference to an environment its creator can't reveal stays unresolved. Every push is
  audited as `sync.push` by `sync:<id>`, with the keys it pushed or removed (roll's names, without
  the prefix) and never a value; a push that fails is audited too.
- **Errors** name keys and the target, never a value: `github owner/app: 404 Not Found`,
  `DB_URL: ${staging.X} doesn't exist`. A failed push keeps the previous `last_digest` and is tried
  again after 5 minutes. Deleting a sync leaves what it pushed at the target. Deleting its
  environment or project deletes the sync.
- **A product's own kinds** (`SyncTarget::internal`: `search-inference`) are made by that
  product's route (`Secrets::create_product_sync`), never `POST …/syncs` (400), and aren't offered
  in `GET …/syncs`'s `kinds`; they're listed, run and deleted like any sync. The product checks its
  own side (roll search: who may change the cluster), and `reveal` on the folder is enough to make,
  run or delete one, as reading the value and pasting it there would be; `admin` may delete one
  too (it doesn't include `reveal`, and stopping a sync sends nothing).
- **Who.** Creating, deleting and running a sync need `admin` on its environment: it sends values
  out of roll. Listing needs `read` and shows the syncs of environments the caller can read.

| Kind | Target | Pushes |
| --- | --- | --- |
| `github` | `{repo: "owner/name", environment?: "production"}` | Repository Actions secrets, or a GitHub environment's secrets, through roll's GitHub App |
| `webhook` | `{url, recipient}` (a base64url X25519 public key) | Every synced value, sealed to `recipient`, POSTed to `url` |
| `notify` | `{url}` | No values: that the environment changed, and which keys, POSTed to `url` (deploy hooks) |
| `vercel` | `{project: "<id or name>", team?: "<team id>", target: ["production" \| "preview" \| "development", …], token_key: "VERCEL_TOKEN"}` | A Vercel project's environment variables (`type: "encrypted"`) for those environments |
| `cloudflare` | `{account: "<account id>", script: "<worker>", token_key: "CLOUDFLARE_API_TOKEN"}` | A Worker's secrets (`secret_text`) |
| `aws-ssm` | `{region, prefix: "/shop/production/", access_key_id_key, secret_access_key_key, kms_key_id?}` | Parameter Store `SecureString`s named `<prefix><name>` |
| `google-secret-manager` | `{project, prefix?, service_account_key}` | one Secret Manager secret per key, `<prefix><KEY>`, each change a new version |
| `azure-key-vault` | `{vault, tenant_id, client_id, client_secret_key}` | one Key Vault secret per key (`_` becomes `-`) |
| `azure-devops` | `{organization, project, group, token_key}`, or `tenant_id, client_id, client_secret_key` instead of `token_key` | an Azure Pipelines variable group's variables, all secret |
| `aws-secrets-manager` | `{region, secret_name: "shop/production", access_key_id_key, secret_access_key_key, kms_key_id?}` | one Secrets Manager secret: the environment's values as a JSON object |
| `search-inference` | `{cluster, endpoint, service, pin}` (made by roll search, see "roll search's inference endpoints") | one key: a roll search inference endpoint's `api_key` (`PUT _inference/{task}/{endpoint}/_update`) |
| `supabase` | `{project_ref, token_key}` | a Supabase project's Edge Function secrets |
| `cloudflare-pages` | `{account, project, environment: "production" \| "preview", token_key}` | a Pages project's environment variables, as `secret_text` |
| `azure-app-configuration` | `{store, label?, prefix?, tenant_id, client_id, client_secret_key}` | one App Configuration key-value per key, `<prefix><KEY>` under `label` |
| `vault-kv` | `{address, mount?, path, namespace?, token_key}`, or `role_id, secret_id_key, approle_mount?` instead of `token_key`; `ca_key?` | one Vault (or OpenBao) KV v2 secret holding the folder's keys |

### `github`

- The repo must be the GitHub repo of a roll repo on the project's canvas (connected through the
  App), checked when the sync is made and before every push, so a project can't write into a repo
  of another tenant that the App happens to reach. Without the App (`ROLL_GITHUB_APP_ID`…),
  creating one answers 400 and says the App isn't configured.
- Each value is encrypted for GitHub with the repo's (or the environment's) public key as a
  libsodium sealed box (`crypto_box_seal`: X25519, XSalsa20-Poly1305, nonce BLAKE2b-24(epk ‖ pk);
  `code_github::seal_secret`), then `PUT …/actions/secrets/{name}` or
  `PUT …/environments/{environment}/secrets/{name}`. Removed names are deleted (a 404 counts as
  done).
- GitHub's names are `[A-Za-z_][A-Za-z0-9_]*` and may not start with `GITHUB_`; a key that would is
  an error naming it (rename it or use `keys`/`prefix`).

### Platform credentials (`vercel`, `cloudflare`, `aws-ssm`, `aws-secrets-manager`, `google-secret-manager`, `azure-key-vault`, `supabase`, `cloudflare-pages`, `azure-app-configuration`, `vault-kv`)

A platform's own credential is never in the sync. The target names **keys of the sync's own
environment and folder** that hold it (`token_key`; for AWS `access_key_id_key` and
`secret_access_key_key`, an IAM user's key the team stores, not a roll bucket's managed key).
They are resolved at each push like any value (references work), never pushed themselves (they're
left out of the synced keys, and `keys` can't list them), and never stored, logged or put in an
error. A credential key that isn't set, is empty or doesn't resolve fails the push with an error
naming it. The calls go to the platforms' fixed hosts (`api.vercel.com`, `api.cloudflare.com`,
`ssm.<region>.amazonaws.com`; `region` is `[a-z0-9-]`), with timeouts, no redirects and at most
1 MiB of an answer read; an error carries the platform's status and error code, never its message.

- **`vercel`**: changed names are upserted in batches of 50 (`POST /v10/projects/{project}/env?upsert=true`,
  `type: "encrypted"`, the target's environments). A removed name is looked up by id
  (`GET …/env`) and deleted (`DELETE /v9/projects/{project}/env/{id}`) only where every
  environment it serves is one of the sync's: a variable that also serves another environment is
  the team's own. `target` may also be a comma-separated string (the dashboard's form). Values are
  at most 64 KB.
- **`cloudflare`**: `PUT /client/v4/accounts/{account}/workers/scripts/{script}/secrets` with
  `{name, text, type: "secret_text"}` per changed name, `DELETE …/secrets/{name}` per removed one
  (a 404 counts as done). Values are at most 5 KB.
- **`aws-ssm`**: `PutParameter` (`SecureString`, standard tier, `Overwrite`, `KeyId` when
  `kms_key_id` is set) and `DeleteParameter` (`ParameterNotFound` counts as done) over AWS JSON 1.1,
  signed with SigV4 by hand from s3-proto's primitives (no AWS SDK in rolld). `prefix` is a path
  that starts and ends with `/`, not under `/aws` or `/ssm`. A value over 4 KB is an error naming
  it.
- **`google-secret-manager`**: `service_account_key` names the key holding a service account's JSON
  key file. rolld signs a JWT with its private key (RS256, `ring`) and trades it at Google's token
  endpoint (`oauth2.googleapis.com`, never the `token_uri` the file names) for an access token, then
  per changed key: `GET` the secret, create it when missing (labelled `managed-by=roll-secrets`,
  automatic replication), and `addVersion` with the value; per removed key, delete the secret only
  when it carries that label. A secret that exists without the label is someone else's: roll never
  writes to it. Errors carry Google's `error.status`, never an answer.
- **`azure-key-vault`**: rolld signs in as the app registration (client credentials at
  `login.microsoftonline.com`, scope `vault.azure.net`), with its client secret from the key
  `client_secret_key` names, then per changed key: `GET` the secret (a name's `_` becomes `-`, since
  Key Vault allows only letters, digits and `-`, and roll's keys never hold a `-`), and `PUT` the
  value with the tag `managed-by=roll-secrets`; per removed key, delete it only when it carries the
  tag. A secret roll deleted earlier is kept by the vault's soft delete: setting it again recovers it
  first, once the deleted secret's tag shows it was roll's. Whether a secret is roll's must be known
  before anything is written or deleted: a refused read stops the push. One that exists without the tag is someone else's, never written. Errors carry Key Vault's
  `error.code`, never an answer.
- **`aws-secrets-manager`**: one secret holding the environment's values as a JSON object (what an
  app reads with one `GetSecretValue`), sent whole on each push, so a removed key is simply gone
  from it. `DescribeSecret`, then `PutSecretValue` on a secret roll created (tagged
  `managed-by=roll-secrets`) or `CreateSecret` (with that tag, and `KmsKeyId` when set); a secret
  that exists without the tag is someone else's, and roll never replaces it. Deleting the sync
  leaves the secret. Same SigV4 by hand (service `secretsmanager`); over 64 KB of JSON is an error.

`supabase`, `cloudflare-pages`, `azure-app-configuration` and `vault-kv` call through rolld's
guarded client (`roll_net::PublicOnly`, no proxy, no redirects, timeouts, at most 1 MiB of an
answer read; private addresses only with `ROLL_CODE_WEBHOOK_ALLOW_PRIVATE`, as webhooks). Their
fixed hosts are `api.supabase.com`, `api.cloudflare.com`, `<store>.azconfig.io` and
`login.microsoftonline.com`, and every part of the target that goes into a host or a path is
checked strictly (a store name is 5–50 letters, digits and `-`, so nothing else reaches the host).
An error carries the status and the vendor's error code where it has one, never an answer.

- **`supabase`**, `{project_ref, token_key}`: a Supabase project's Edge Function secrets, through
  the Management API with a personal access token (`sbp_…`; fine-grained:
  `edge_functions_secrets_write`). Changed names are upserted in batches of 100
  (`POST /v1/projects/{ref}/secrets`, `[{name, value}]`); removed names, the ones this sync pushed
  before, go in one `DELETE` of the same path with their names. Functions read a new value at once,
  no redeploy. Supabase keeps `SUPABASE_` for the variables it injects (`SUPABASE_URL`…), so a key
  that would push as `SUPABASE_…` is refused when the sync is made (and named if one is added later):
  leave it out with `keys`, or set a `prefix`. Supabase answers errors as a message without a code,
  so its errors carry the status alone.
  ```sh
  roll secrets -e production syncs add supabase --target project_ref=abcdefghijklmnopqrst \
    --target token_key=SUPABASE_ACCESS_TOKEN
  ```
- **`cloudflare-pages`**, `{account, project, environment, token_key}`: a Pages project's
  environment variables for `production` or `preview` (its two environments), for builds and Pages
  Functions. One `PATCH /client/v4/accounts/{account}/pages/projects/{project}` per push sets
  `deployment_configs.{environment}.env_vars`: changed names as `{type: "secret_text", value}`,
  removed ones (pushed by this sync before) as `null`, which is how Cloudflare deletes a variable;
  every other variable stays as it is. The token is handled as `cloudflare`'s (a key holding an API
  token, with Pages Write), and errors carry Cloudflare's error codes. A value is at most 5 KiB. A
  change reaches the next deployment.
  ```sh
  roll secrets -e production syncs add cloudflare-pages --target account=023e105f4ecef8ad9ca31a8372d0c353 \
    --target project=shop-web --target environment=production --target token_key=CLOUDFLARE_API_TOKEN
  ```
- **`azure-app-configuration`**, `{store, label?, prefix?, tenant_id, client_id,
  client_secret_key}`: one key-value per key in an App Configuration store, named `<prefix><KEY>`
  (`shop:DB_URL`), under `label` (no label by default). rolld signs in as `azure-key-vault` does
  (client credentials at `login.microsoftonline.com`), for the scope
  `https://<store>.azconfig.io/.default`; the app registration needs App Configuration Data Owner on
  the store. Per changed key: `GET /kv/{key}?label=…`, then `PUT` the value tagged
  `managed-by=roll-secrets` (API `2023-11-01`); per removed key, `DELETE` it only when it carries
  the tag. A key-value that exists without the tag is someone else's: roll never writes to it, and
  a refused read stops the push. A key-value is at most 10 KB. Errors carry App Configuration's
  problem `type` (`key-locked`) or Entra's `error`. **Values are plain key-values.** App
  Configuration's own guidance is to keep secrets in Key Vault and reference them from the store
  (Key Vault references); roll writes the values themselves, which suits configuration and
  non-critical values read through the store. For secrets, sync to Key Vault with `azure-key-vault`
  and add references to those secrets in the store.
- **`vault-kv`**, `{address, mount?, path, namespace?, token_key | role_id + secret_id_key,
  approle_mount?, ca_key?}`: one secret on a HashiCorp Vault or OpenBao KV v2 mount (`secret` by
  default) holding the folder's keys, written whole on each push
  (`POST {address}/v1/{mount}/data/{path}` with `{"data": {…}}`), so a removed key is simply left
  out of the next version. The address is the team's own server, fetched like a webhook: a public
  `https://` URL (`PublicOnly`, no redirects), trusting the CA in `ca_key` (PEM, a key of this
  environment and folder) when the server's certificate isn't publicly trusted. It signs in with a
  token (`token_key`) or AppRole (`POST /v1/auth/{approle_mount}/login` with `role_id` and the
  secret id in `secret_id_key`); `namespace` goes as `X-Vault-Namespace` (Vault Enterprise, HCP).
  Before writing, roll reads the secret's metadata: a new secret is marked with the custom metadata
  `managed-by=roll-secrets` first, and one that exists without that mark is someone else's, which
  roll never replaces. The policy it needs: `create`, `update` on `{mount}/data/{path}`, and
  `read`, `create`, `update` on `{mount}/metadata/{path}`. Vault's errors are messages, so roll's
  carry the status alone. The synced values come to at most 512 KiB as JSON.
  This is the way off Vault one service at a time: move a folder's values into roll (`roll secrets
  import --from vault`), sync them to a path of roll's own, point the service at that path, and
  later at roll itself (`roll run`, the SDKs) whenever it suits; Vault keeps serving current values
  until then. A secret Vault's own tooling made is never touched: a sync writes only to its own.
  ```sh
  roll secrets -e production syncs add vault-kv --target address=https://vault.example.com:8200 \
    --target path=shop/production --target role_id=db02de05-fa39-4855-059b-67221c5c2f63 \
    --target secret_id_key=VAULT_SECRET_ID
  ```

### `render`, `netlify`, `fly`, `railway`, `heroku`, `digitalocean`, `circleci`, `bitbucket` and `terraform-cloud`

- **`render`**, `{service, token_key}`: a Render service's own environment variables (not a linked
  environment group's). A changed key is set (`PUT /v1/services/{id}/env-vars/{key}`), a removed one
  deleted; the key holding an API key is a secret of this environment and folder.
- **`netlify`**, `{account, site, context?, token_key}`: a Netlify site's variables, one deploy
  context's values (`all` by default; `production`, `deploy-preview`, `branch-deploy`, `dev`). A
  changed key sets that context's value (or creates the variable with it), a removed key deletes
  the variable.

- **`fly`**, `{app, token_key}`: a Fly.io app's secrets through Fly's GraphQL API, as flyctl sends
  them (`setSecrets` for changed keys, `unsetSecrets` for removed ones; a macaroon deploy token goes
  as `FlyV1 …`, anything else as `Bearer …`). Setting secrets makes a release, as `fly secrets set`
  does.
- **`railway`**, `{project, environment, service?, token_key, token_kind?}`: a Railway service's
  variables in one environment (the environment's shared variables without `service`), through its
  public GraphQL API (`variableCollectionUpsert`, `variableDelete`). The token is a project token
  (`Project-Access-Token`) by default, or an account token with `token_kind: "account"`.
- **`circleci`**, `{project: "gh/acme/web", token_key}`: a CircleCI project's environment variables
  (`POST …/project/{slug}/envvar` per changed name, `DELETE` per removed one, `Circle-Token`).
  CircleCI keeps no tag on a variable, so only names this sync pushed before are ever removed.
- **`bitbucket`**, `{workspace, repo, environment?, token_key}`: a repository's Pipelines variables,
  or one deployment environment's (its uuid), all secured. The sync lists them for their uuids
  (following `next` only on Bitbucket's API host), then `PUT`s an existing one, `POST`s a new one and
  `DELETE`s a removed one.
- **`terraform-cloud`**, `{workspace_id, category?, token_key}`: a Terraform Cloud workspace's
  variables (`category` `env`, the default, or `terraform`), all `sensitive`, over its JSON:API
  (`…/workspaces/{id}/vars`). Terraform Cloud keeps no tag on a variable, so roll describes its own
  as `managed by roll secrets` and never writes to or deletes one without that description.
- **`digitalocean`**, `{app, component?, token_key}`: an App Platform app's environment variables,
  app-wide or one component's (a service, worker, job, function or static site, by name). The sync
  reads the app spec, replaces only the keys it pushes (as encrypted `SECRET`s, run and build time),
  drops the keys it removes, leaves every other variable as it came (already-encrypted values go
  back unchanged), and puts the spec back. Updating the spec redeploys the app.
- **`heroku`**, `{app, token_key}`: a Heroku app's config vars, in one `PATCH /apps/{app}/config-vars`
  per push (changed names with their values, removed ones as `null`). A failure names Heroku's error
  `id`, never the answer, which echoes the vars.

Like Vercel's, they use the platforms' fixed API hosts, and a GraphQL answer's `errors` fail the
push by code, never with a value.

### `gitlab`

A GitLab project's CI/CD variables in one environment scope, `{project, environment_scope?,
token_key, base_url?}`: the project's id or `group/project`, the scope (`*` by default), the key of
this environment and folder holding an access token with the `api` scope, and a self-managed
instance's public URL (gitlab.com by default; fetched like a webhook, `PublicOnly`). A changed key
is updated in that scope, or created; a removed one is deleted. Values are raw (no `$` expansion),
and masked when GitLab can mask them (8 characters or more, no spaces), so they stay out of job logs.

### `azure-devops`

An Azure DevOps variable group's variables, `{organization, project, group}` (`dev.azure.com/acme`,
its project's name or id, the group's name), all pushed as secret variables, which pipelines that
link the group read (`variables: [{ group: shop-production }]`; a script gets a secret only
through its step's `env:`, as Azure Pipelines does for every secret). It signs in one of two ways:

- `token_key`: the key holding a personal access token with **Variable Groups (Read, create, &
  manage)**, sent as Basic auth;
- `tenant_id`, `client_id` and `client_secret_key`: a Microsoft Entra service principal (client
  credentials at `login.microsoftonline.com`, for Azure DevOps' own resource
  `499b84ac-1321-427f-aa17-267ca6975798`), as `azure-key-vault` signs in. Microsoft is phasing
  personal access tokens down for automation; the principal has to be a user of the organization
  with the Administrator role on the group (Pipelines → Library → the group's Security).

```sh
roll secrets -e production syncs add azure-devops --target organization=acme --target project=shop \
  --target group=shop-production --target token_key=AZURE_DEVOPS_PAT
```

Each push finds the group by name (`GET …/{project}/_apis/distributedtask/variablegroups?groupName=`,
the one whose name matches exactly) and puts it back whole (`PUT …/{organization}/_apis/distributedtask/variablegroups/{id}`,
API 7.1), since Azure's update replaces every variable at once: every key this sync manages is
set with its value and `isSecret: true` (all of them on each push, never a secret without its
value), removed ones (keys this sync pushed before) are dropped, and the group's plain variables go
back as they came, its description and project references too. **Give the sync a group of its
own**: Azure never returns a secret variable's value, so a secret roll doesn't manage couldn't be
written back as it was, and a group holding one is refused, naming it, rather than risk clearing
it. Azure's variable names are case-insensitive, and so is the match. Roll creates no group: one that doesn't
exist fails the push (create it in Pipelines → Library), and so does one linked to a Key Vault
(sync to the vault instead). A key named with a prefix Azure Pipelines keeps for itself
(`endpoint`, `input`, `secret`, `path`, `securefile`, any case: `SECRET_KEY` too) never reaches a
task, so it's an error naming it: leave it out with `keys`, or set a `prefix`.

`organization` is 1–50 letters, digits and `-`; `project` up to 64 letters, digits, spaces and
`-_.`; `group` up to 128 characters without `*` (Azure's filter takes it as a wildcard). Calls go
only to `dev.azure.com` (and `login.microsoftonline.com`), through the guarded client:
`PublicOnly`, no redirects, at most 1 MiB of an answer, timeouts. Errors carry Azure's status and
`typeKey`, never an answer. A refused token answers with a sign-in page (203), which the error
says. Two writers can race: something that edits the group between roll's read and its write
loses that edit (Azure's API has no precondition to send); the next push sets roll's keys again.

### `kubernetes`

One Secret in a cluster, `{server, namespace, secret, token_key, ca_key?}`: the API server's
public `https://` URL (fetched like a webhook: `PublicOnly`, no redirects), the namespace and the
Secret's name, and the keys of this environment and folder holding a service account token (with
`create`, `get` and `update` on that Secret) and, optionally, the cluster's CA in PEM. Each push
writes the Secret's `data` as exactly the synced keys (removed ones are gone), labelled
`app.kubernetes.io/managed-by=roll-secrets` and annotated with the project, environment and
folder. roll never replaces a Secret without that label, so it can't clobber one of the team's own.
The synced values must fit a Secret's 1 MiB. Pods read it as usual (`envFrom.secretRef`); a pod
restart or a reloader picks up a change.

#### Pulling instead: External Secrets Operator

A cluster whose API server isn't public (a private EKS endpoint, a cluster on a private network)
can pull instead, with [External Secrets Operator](https://external-secrets.io)'s webhook
provider: the cluster calls roll, and roll never calls the cluster.
`roll secrets -e production [--path /api] kubernetes [--namespace shop] [--url https://…] | kubectl
apply -f -` makes a token for the environment and folder and prints all three objects below; the
dashboard's Export menu has the same ("External Secrets Operator…", the manifests shown once). One
`SecretStore` per project serves a whole environment (`dataFrom`) and single keys (`data` with
`property`). For a tokenless setup, roll's own operator (#36) pulls with the cluster's
identity instead; this is for clusters that already run ESO for their other managers:

```yaml
apiVersion: v1
kind: Secret
metadata:
  name: roll-token
  labels:
    external-secrets.io/type: webhook   # ESO reads only labelled Secrets
stringData:
  token: rlt_…                          # roll secrets -p shop tokens create k8s --for-env production
---
apiVersion: external-secrets.io/v1
kind: SecretStore
metadata:
  name: roll-shop
spec:
  provider:
    webhook:
      url: "https://roll.example/v1/secrets/projects/prj_…/environments/{{ .remoteRef.key | urlquery }}/values?dynamic=false&strict=1"
      headers:
        Authorization: "Bearer {{ .auth.token }}"
      result:
        jsonPath: "$.values{{ with .remoteRef.property }}.{{ . }}{{ end }}"
      secrets:
        - name: auth
          secretRef:
            name: roll-token
---
apiVersion: external-secrets.io/v1
kind: ExternalSecret
metadata:
  name: shop-production
spec:
  refreshInterval: 1h
  secretStoreRef: { name: roll-shop, kind: SecretStore }
  target: { name: shop-production }
  dataFrom:
    - extract: { key: production }        # every key; or data: [{secretKey: url, remoteRef: {key: production, property: DATABASE_URL}}]
```

- The token is a project token for the environment (`--for-env`); each refresh is one audited read
  by it (`secrets.export`), so a short `refreshInterval` fills the audit log.
- A folder is `&path=/api` on the store's URL (a store per folder).
- `dynamic=false`: ESO can't renew a lease, so a dynamic key would be a new credential on every
  refresh that lapses on its own. With it, a dynamic key is left out (named in `dynamic`); link it
  as `managed` (one credential roll rotates) or `live` instead, which ESO's next refresh follows.
- `strict=1`: a key that doesn't resolve (a reference to a key that's gone, a live link whose
  resource doesn't answer) fails the whole read with 409 `unresolved`, naming the keys. ESO keeps
  the Secret it has and marks the ExternalSecret not synced, instead of writing an empty value
  over a working one. Dynamic credentials such a read issued are revoked with it.
- `urlquery` on the key: whoever may make an ExternalSecret can't add their own query
  (`staging/values?dynamic=true&x=`) to the store's URL.
- Checked against ESO 2.15 on k3s 1.33: `dataFrom` and `data` from one store, and a change in roll
  reaching the Secret on the next refresh.

### `webhook` and `notify`

Both POST JSON to a public HTTPS `url` the creator chose: fetched through the `PublicOnly` resolver,
a literal private host refused, no redirects, 15 s timeout, at most 64 KiB of the answer read (and
ignored). Any 2xx is success. `ROLL_CODE_WEBHOOK_ALLOW_PRIVATE` (local development only) allows
`http` and private addresses.

Each sync of these kinds has a **signing secret** (`whsec_…`), made when the sync is created, shown
once in the answer, and stored sealed with the project's data key. Every request carries:

```
X-Roll-Sync: syn_…
X-Roll-Timestamp: <unix seconds>
X-Roll-Signature-256: sha256=<hex HMAC-SHA256(secret, "<timestamp>.<body>")>
X-Roll-Delivery: dlv_<32 hex>   (the same on every retry of this delivery)
```

It's the scheme of every roll webhook (roll code's, roll-access's audit sinks), so one receiver
verifies them all.

- `webhook` body: `{sync, project, environment, path, digest, at, changed, removed, context,
  sealed}`. `sealed` is a `roll-seal-v1` box (see "Runtime resolve") to `recipient` of the JSON
  object of every synced name and value; `context` is `roll-secrets-sync:<sync id>`. Only the
  holder of the recipient's secret key reads the values, so a team can write a sync for any
  platform without roll knowing it.
- `notify` body: `{sync, project, environment, path, digest, changed_keys, removed_keys, at}`. No
  value, sealed or not, and nothing is resolved or revealed for it: a deploy hook ("redeploy when
  production's secrets change").

## Governance

### Access

The roll-wide permission model (docs/permissions.md, `crates/roll-access`, branch
`feature/roll-mcp`) decides who may do what: rolld's gate classifies each request by its route
pattern and method into the product `secrets`, a resource (`{project, environments, paths}`) and one
action:

| Action | Routes |
| --- | --- |
| `read` | names, metadata, versions, history, compare, folders, imports, exports (listing), what's shared with the project, requests, links, leases, required keys, listing syncs, insights, the audit log |
| `reveal` | `…/values`, `…/secrets/{key}/value`, `…/secrets/{key}/totp`, `…/versions/{v}/value`, `…/requests/{id}/values`, `…/resolve`, renewing a lease (whoever could read the credential), creating a one-time share (how a value leaves roll) |
| `write` | set, batch changes, rotate, rollback, promote, folders, imports (a project's own), create an environment, open or apply a request |
| `delete` | a secret, an environment, ephemeral environments by label, a lease |
| `admin` | rename or reorder environments, protection and approval policy, reviewing a request, syncs (create, delete, run), tokens and identities (listing too), audit sinks, revoking another's one-time share, exports (sharing a folder with another project), imports from another project, purge and retention, required keys (setting), making a trusted preview |

`POST /v1/secrets/login` (workload identities) and the one-time share links (`/v1/secrets/shares/…`)
are public: the OIDC token, or the key in the link, is the credential.

Values are returned only by the `reveal` routes, so the gate can classify a request without reading
its body. A route that touches a second resource (the source of a promote, a reference into another
environment, a path inside a batch) checks it in process through the same evaluator. The rules
that follow from it:

- **Nothing reaches an environment its caller can't read.** A reference, an import and an ephemeral
  environment's base each show through only where the caller may read (for a listing) or reveal
  (for values) the other environment. Making an ephemeral environment needs `reveal` on its base.
- **Links are an admin's choice.** A link reaches a resource's credentials as the operator, so
  setting one (or promoting one) needs `admin` in that environment, and so does rotating a
  resource's own credential (`live` links: it affects every environment that links it). A link
  works only while its resource is on the project's canvas, checked on every read, issue and
  rotation.
- **Protected environments.** Changing their imports needs `admin` (it changes what they resolve to
  without a change). Checks and sandboxes resolve as a consumer that may not touch any protected
  environment, so none reaches them through a reference, an import or a base either.
- **Folders.** A caller limited to some folders sees only their changes, change items, requests,
  audit rows and the leases of their secrets.
- **Fingerprints** are shown only to a caller who may reveal the value: a writer could otherwise
  test guesses against values it can't read. The grid's and compare's statuses still say whether
  two environments match, to a caller who can read both.
- Token scopes name environments by slug: a token for `pr-117` reaches a new `pr-117` made after the
  old one was deleted (it needs `reveal` on the new one's base to make it).

rolld's gate decides each request from the route (roll-access's catalogue). The checks it can't
make from the request alone (a second environment, a path inside a batch, each finding of a
filtered answer) go through one trait in `crates/roll-secrets/src/access.rs`, which every caller
carries:

```rust
pub trait Grants: Send + Sync + Debug {
    fn allows(&self, action: Action, at: &Scope<'_>) -> bool;     // may they, here
    fn project(&self) -> Option<&str>;                            // the one project they're limited to
    fn untrusted(&self) -> bool;                                  // code nobody vetted
    fn reads_code(&self, project: &str, repo: &str) -> bool;      // health's code check
}
```

It is the operator (`Everything`), a roll-access grant (`GrantGrants`, for `rlt_…` tokens and
`rlo_…` OAuth connections: `ask` inside the product is a no). A project's token is such a grant
with one secrets rule (`POST /v1/access/tokens`), and a workload identity's login is a `workload` grant.

Secrets accepts the operator (`roll_auth::Operator`, or any request when rolld has no operator
token), signed-in people on their own projects (below), and roll-access's grants: there is one
permission system (docs/permissions.md). A token for CI or a service is a roll token (`rlt_…`)
with one rule on the project's secrets (environments or all, folder prefixes or all, the actions
above), an optional expiry and client addresses (up to 20 addresses or CIDRs; from anywhere else
it's refused), made through `POST /v1/access/tokens`: `roll secrets tokens create … --allow-ip
203.0.113.0/24`, or the project's Access tab, which lists them from `GET
/v1/access/tokens?product=secrets&resource=<project>` (one at a time: the list also holds tokens
made in the access builder, so nothing here revokes them in bulk). The API reads a rule without
`environments` as every environment and a grant without `expires_at` as one that never expires
(unless roll's token policy requires one), so both clients always send them: the CLI one
environment (`--for-env`, else the directory's own, never a protected one unless named) and 90
days, with `--all-environments` and `--no-expiry` only when asked; the Access tab's form starts from
`development` (else the first unprotected environment) and 90 days. roll-access stores them hashed,
looks them up on every request and never caches them; OAuth connections (`rlo_…`) reach secrets
the same way. rolld's gate classifies each request, and secrets asks the same `Grant` for every
second check (`GrantGrants`); a read of the project as a whole (its environments' names, health,
findings) takes part for a rule limited to some environments or folders, answering only what it
reaches. An identity can be pinned to client addresses too: a login from elsewhere is refused, and
the token it gets carries the same list.

#### People (signed-in accounts)

Someone signed in to roll (docs/accounts.md) who isn't an admin manages the secrets of the
workspace projects they own (an admin's session is the operator):
rolld's gate lets a session through `/v1/secrets` (`Access::UserOrCredential`: a session, or a
request carrying a credential secrets checks), with `X-Roll-CSRF` on writes. Their caller is
`OwnProjects`: everything on their own projects as the workspace has them on this request, nothing
on anyone else's, which answers 404 as the workspace does (sharing a folder with a project they
can't see is refused the same way as with one that doesn't exist). Search, findings and
`health --all` keep only their projects. The audit log names the person by user id (`usr_…`), and
their personal overrides are theirs. The deployment key's state (`GET /v1/secrets/health`) is the
operator's.

What a person made acts later only while they can sign in and still reach its project (an admin
then, or its owner now): their tokens, any token one of those made, their identities' logins,
their syncs and the references they chose for another product (`bind_ref`). What an admin's
session makes is the person's (`Caller::maker`), though the audit log names it `operator`. rolld asks roll-accounts on every use (`People`); a lookup that fails is an error, not
a disabled account. A sync a person made pushes as them, as they are then.

Anyone may put any resource on their own canvas, so a project reaches a resource only where the
workspace trusts it to (`Workspace::trusts`, docs/workspace.md "What a canvas stands for"): the
operator's and an admin's projects their whole canvas, a person's only repos under their own roll
code owner (`ada/…`).

- **Links, audit sinks and syncs to roll's products** (`members`) take only what the project is
  trusted for: a link reaches a resource's credentials as the operator. A person's project can link
  its own rotators. This lifts per product once each says who owns a resource.
- **roll code and roll previews** (checks, previews and sandboxes getting values, leak blockers,
  missing-key notices) count a project only for repositories it's trusted for: otherwise anyone
  could hand a stranger's previews their variables (say, a `DATABASE_URL` pointing at their own
  server), or watch a stranger's landings for their values.

### Read quotas

A token or a person reads values at most 600 times a minute, counted across every node in the
database they share (the operator sets it:
`ROLL_SECRETS_READS_PER_MINUTE`, 0 for no limit), as Vault's rate-limit quotas: past that,
`…/values`, `…/resolve` and a key's reveal answer 429 (`rate_limited`) for the rest of the minute,
before anything is decrypted, issued or audited. roll's own runtimes (apps, previews, checks,
syncs, workload identities' logins) and the operator aren't held to it. A service that reads in a
loop should cache (the clients' `cacheTtl`) or follow the digest (`watch`).

### Reach

`GET …/environments/{env}/reach?path=&key=` (`roll secrets reach [KEY]`, a key's menu "Who can
read it…" in the dashboard) answers, for a folder or one key, every way its values can leave roll:
before rotating after a leak, or for a review. It's a project admin's, since it lists the project's
credentials as the tokens list does. Names and metadata only, never a value:

- **Who could**: the tokens and workload identities whose scope covers it, each with what it may
  do there (read, reveal, write, admin; decided by the same scope check requests are), when it was
  last used and when it ends; and whether code roll hasn't vetted (checks, sandboxes, previews)
  can read it, which is so unless the environment is protected.
- **Where it goes by itself**: syncs of that folder (and that key), projects the folder is shared
  with, and this project's folders that import it.
- **Where it goes by itself** also counts folders that import it and environments based on it
  (previews), and theirs in turn, as reads follow them (up to 8 steps and 256 folders; `truncated`
  says when there were more). A read crosses into another project once, from here to the project
  it's shared with (while it's shared), and never on into a third; that project is named only as
  `project prj_…`, never its environments. Syncs of this project's importing folders are listed.
- **Who did**: everyone who revealed, exported, resolved or synced it in the last 30 days, with
  how often and when last (the audit log), here or through those folders (`operator (through
  staging:/)`); another project's readers are named by the project, not their tokens. Reads through
  a `${…}` or `roll://` reference in another key are recorded where they happened and aren't
  counted: the answer says so (`uncounted`).

Who could lists roll-access's live tokens (`rlt_…`) by what each may do here, and workload identities
by their identity rather than each login. OAuth connections (`rlo_…`) aren't listed yet.

It also lists what the folder has handed out that's still out: dynamic credentials issued for
its keys and not yet revoked (`leases`), open one-time shares made from them (`shares`, as their
makers said with `from`), and how many of the project's open shares don't say where their value
came from (`unattributed_shares`). `frozen` says whether it's frozen, and by whom and why.

#### Cutting it off

`POST …/environments/{env}/reach/cut-off?path=&key=` with `{why, tokens?, identities?, leases?,
shares?, syncs?, freeze?}` (`roll secrets reach [KEY] --cut-off --why "…" [--freeze]`, the reach
dialog's "Cut off…") cuts everything reach lists, in one call. It's a project admin's, as reach is.
Each part is on unless it's `false`, except `freeze`, which is off unless it's `true`. It reads
reach as the caller sees it (the list the confirmation showed), then:

- **Tokens**: roll-access's grants that reach it are revoked through roll-access's own bulk revoke
  (`Access::revoke_many`, called in process through `Workloads::revoke_many`): one write per call,
  all or none, at most 500 ids a call, each grant with `revoked_why` (`cut off from
  production:/ STRIPE_KEY: <why>`) and its own audit row in roll-access's log. More than 500 go in
  several calls; `batches` says how many.
- **Who revokes which token.** A project's admin (a person who owns the project, or a token with
  `admin` there) revokes only tokens confined to the project: every rule of the grant, for every
  product, names this project (`{project}`, or secrets' `{id}`), with no `*`, no other project, and
  no `ask_otherwise`. `access::confined_to` reads the rules as data; anything it can't prove
  confined counts as reaching beyond. Those stay, listed under `skipped` (`{id, name, why}`, `why`
  "reaches beyond this project: the operator revokes it"), so the admin can ask the operator, and
  reach marks each with the same `skipped` before the confirmation. The operator (and an admin
  person) revokes every token that reaches the folder, theirs included. Without this, one
  project's admin could revoke the operator's all-projects tokens and cut off every other
  project's CI. Identities and their logins are the project's own (each login is one rule naming
  this project), so any of its admins cuts them off.
- **Identities** that reach it are disabled (`disabled_at`, `disabled_why`): a login answers 403
  until an admin enables it again (`PATCH …/identities/{id}` with `{disabled: false}`). Their live
  logins' grants go through the same bulk revoke (`logins` counts them). A login that races the
  cut-off can't keep its token: it's recorded only while the identity is still enabled.
- **Leases**: dynamic credentials issued for its keys (or the key) end, each revoked at its
  product outside the transaction, as `DELETE …/leases/{id}` does; one the product refuses goes
  back to the sweeper and is named under `failed`. A retired managed credential waiting out its
  grace isn't tied to a folder and ends on its own; rotate the key to replace the current one.
- **Shares**: open one-time shares made from its keys expire, and the projects the folder is
  shared with stop reading it: its exports are removed (`projects`), so their importing folders
  lose its values at once. An export is the whole folder's, so a cut-off of one key removes the
  folder's exports too; share it again once the key is rotated. Shares that don't say where they
  came from are left alone (revoke them by hand if they might hold it).
- **Syncs** that push it (its own, and this project's importing folders') are paused
  (`paused_at`, `paused_why`): nothing is pushed, and running one answers 409 `paused`, until an
  admin resumes it (`PATCH …/syncs/{id}` with `{paused: false}`). What changed meanwhile goes out
  on the next pass.
- **Freeze** (off by default): reads of the folder's values answer 423 `frozen`, the shape of
  `kms_denied`, to everyone but its admins, also through a folder that imports it, an environment
  based on it, a reference, or the history. Admins still read and write, so they can rotate.
  An admin here is who the caller is, not only what it may do: the operator, a signed-in person,
  or a roll-access token with `admin` there. roll's own readers never pass a freeze, though they
  may do everything in-process: apps' services, roll code's checks, sandboxes and previews (an
  untrusted caller), other products' `roll://` references, workload identities' logins, share
  links and syncs (a sync made after the freeze pushes nothing: its pushes fail with `frozen`
  until the freeze is lifted). Names and metadata stay readable. `DELETE …/reach/cut-off?path=` lifts it. A freeze is the
  folder's, not the folders under it, as reach is.

What's roll secrets' own (identities, shares, exports, syncs, the freeze) changes in one
transaction; revocations happen outside it. One audit row (`reach.cut_off`) names what was cut and
why, beside the rows the revocations write themselves. The answer lists what was cut (`tokens`,
`identities`, `logins`, `leases`, `shares`, `projects`, `syncs`, `frozen`), what was left to the
operator (`skipped`), and what couldn't be, each with why (`failed`), without stopping the rest. It's idempotent: a second call cuts what's
left (a bulk revoke that failed, a lease its product refused).

#### During an incident

A value leaked (a log, a laptop, a fork), or a credential that reads it did:

1. **Look.** `roll secrets reach KEY` (or the key's "Who can read it…"): who could read it, where
   it goes by itself, who did in the last 30 days.
2. **Cut off.** `roll secrets reach KEY --cut-off --why "INC-123: STRIPE_KEY in a CI log"`. Add
   `--freeze` when nobody but the admins should read the folder until it's rotated (services that
   read it start failing with 423, loudly). A project's admin revokes the project's own tokens; a
   token that reaches other projects too is left and listed as `left` (`skipped`): hand those
   names to the operator, who runs the same cut-off and revokes them.
3. **Rotate.** A managed key: `roll secrets rotate KEY` (or the dashboard's Rotations). A plain
   value: issue a new one at its vendor and set it. Admins can do both while it's frozen.
4. **Restore.** `roll secrets reach --unfreeze`, resume the syncs (`PATCH …/syncs/{id}`), enable
   the identities that should log in again, share the folder again, and make new tokens for what
   needs them. The audit log has every step.

Measured (release, 2026-11-14, `examples/bench.rs`, load average about 52): at its cap, 300 folders
importing production (it follows 256), each read once, reach takes 5–7 ms on SQLite and 120–150 ms
on Postgres. The audit is read once per project and filtered in memory (it was a query per folder:
55–89 ms and 254–302 ms); what's left on Postgres is the walk's two queries per folder.

### Decoys

A decoy key (a honeytoken, as GitGuardian's are) is a credential made to be found. It's stored
where an intruder would look, and any use of it raises an alert. `POST …/environments/{env}/decoys
{key, kind, path?, resource?}` (`roll secrets decoy add KEY --kind rlt [--path /ci]`, `--kind
postgres --resource <database>`, the folder's menu "Add a decoy…") makes a new key whose value is
a decoy credential:

- **`rlt`**: a roll token that roll-access made as a decoy (docs/permissions.md, "Decoy tokens").
  It allows nothing, whatever its rules say. Any use is refused as an unknown token is, so whoever
  holds it learns nothing. roll-access records the use on the token, writes its own audit row,
  mails the decoy's owner (whoever planted it) and the admins, and tells roll secrets
  (`Access::on_decoy`). roll secrets keeps no token store: rolld mints the decoy through
  `Workloads::mint_decoy`.
- **`postgres`**: a connection string for a role of `resource`, a roll Postgres database the
  project may link (on its canvas, as a link's). It's the root branch's connection string with a
  role and password shaped like the ones links issue (`roll_` and 12 letters and digits, 32
  characters). The role can never log in, and roll keeps no password for it: the password exists
  only as the key's value.
  - rolld's postgres `LinkProvider` asks roll Postgres for the role through a typed route,
    `POST /v1/projects/{id}/decoy-roles {role}` (`DELETE …/decoy-roles/{role}` drops it). roll
    Postgres makes it `NOLOGIN`, with no password, on the root branch (refused if the name exists),
    and keeps it in `decoy_roles (project_id, role)` with a random SCRAM salt of its own.
  - Its proxy checks every login's role against that table after the endpoint resolves and
    before it wakes a compute: one statement over three primary keys (7 µs in a debug build under
    load, against 88 ms for the whole login). A decoy's login is answered by the proxy itself, on every endpoint of the database
    (branches and replicas too), and never wakes a compute.
  - The answer is Postgres' own, byte for byte: the SCRAM exchange as Postgres runs it for a
    role that doesn't exist (the role's own stable salt, a 24-character server nonce, 4096
    iterations), then `FATAL 28P01 password authentication failed for user "…"` with Postgres'
    source file, line and routine for the endpoint's major version (16, 17, 18). Every malformed
    message a prober can send gets the error Postgres would give (`pg-proxy/src/decoy.rs` ports
    `CheckSASLAuth`, `scram_exchange` and its parsers, and `NegotiateProtocolVersion`). Once the
    proxy has seen the endpoint's compute refuse a wrong password, it answers with those exact
    bytes (its language and line). It answers as late as the compute would: as long as the
    endpoint's last start when it's suspended, plus the time a running compute takes to answer.
    `pg-control/tests/e2e.rs` compares each path with a compute's answers to a missing role.
  - Each login is told to rolld (`Control::on_decoy_login`: the endpoint, the client's address
    from the PROXY header, the time), which hands it to `Secrets::decoy_used` as `on_decoy` does
    for tokens. The alert names the endpoint, never the role.
  - Someone who can read the database's catalog (its owner) can see the role is `NOLOGIN`; the
    decoy is for whoever can't.
- `aws` isn't here: an AWS decoy needs an AWS account and CloudTrail.

The key is an ordinary change (as a `PUT` of the key makes), so it reads, resolves, exports and
syncs like any other key. Only the project's admins (`admin` over the whole project) see that it's
a decoy:
- the `decoy` on `Secret` in the folder's list (the dashboard's "decoy" badge, and "used N×" in
  red once it's been used);
- `decoy_used` in health;
- its rows in the audit log (`secrets.decoy` when it's planted, `secrets.alert.decoy_used` on each
  alert).

Everyone else gets the same answers as for any key, without those fields and rows. Nothing marks it
in the grid, reach or a sync's target: syncing a decoy into CI, or exporting it into an old config,
is the point. In a protected environment it lands without a change request. It changes no value
anyone has, and only an admin (who could lift the protection) may plant one.

**What to plant where.** Put it somewhere only an intruder reads, under a name a real credential
would have (`LEGACY_DEPLOY_TOKEN`, `ROLL_ADMIN_TOKEN`). For example:
- a CI system's variables, synced from a folder;
- an old branch's `.env`, or a backup's config;
- a wiki page about deploys;
- a developer laptop's shell profile.

One decoy per place tells you which place leaked.

**When it's used.** roll secrets counts every use and raises `decoy_used` at most once an hour per
decoy:
- an audit row (`secrets.alert.decoy_used`, by `roll:decoys`, with the source address), which the
  environment's Activity shows under Alerts and audit sinks carry to a SIEM;
- a notice to the environment's notify URL, if it has one (the same outbox and retries as read
  alerts);
- `decoy_used` first in health, until the decoy ends.

Each names the key, the environment, the folder, when and the address it came from, and the route
it called. It never names a value.

**What to do.** Someone has read the place the decoy was planted:
1. Cut off whatever can reach that place (the CI system's tokens, the repository's deploy keys, the
   laptop's sessions).
2. Rotate the real credentials stored beside the decoy (`roll secrets rotations --rotate-due`, the
   Health view).
3. Delete the decoy key and plant a new one.

**Ending.** Changing a decoy key's value, linking it, or deleting it (its folder, its environment or
its project too) ends the decoy. The write marks it ended in its own transaction, and its token is
revoked (or its role dropped from the database and from the proxy's table) right after, outside the
transaction (roll-access and roll Postgres are other stores). The sweeper retries what fails. A copy promoted or rolled back into another key holds a token that stops working
once the decoy ends.

### Workload identities (OIDC)

A CI job or a cluster logs in with its own short-lived OpenID Connect token instead of holding a
long-lived token, as Infisical's OIDC auth, Doppler's identities and Vault's JWT auth do: nothing
long-lived sits in a CI system's settings, and a leaked log or fork can't reuse a login past its
minutes.

- **An identity** (`…/identities`, admin) names an `issuer` (the token's `iss`), an `audience`
  (`aud`, default this server's URL from the CLI and the dashboard, so a token minted for one roll
  server isn't good at another), a `subject` (`sub`, or a pattern of it where `*` stands for
  any run of characters), up to ten other `claims` that must match (`ref`, `environment`,
  `repository_owner_id`…; see "Claims" below), and the scope of what a login gets: environments, folders and actions
  like a token's (never more than its creator has) for `ttl_minutes`, 5–60 (default 15).
- **Shared issuers.** GitHub Actions, GitLab.com, Terraform Cloud, Buildkite and Google sign for
  every tenant they host, so an identity of theirs must name one tenant literally in its subject
  (`repo:acme/web:…`, `project_path:group/project:…`, `organization:acme:…`, a Google service
  account's numeric id exactly): `repo:*` would let anyone's job in. Azure's issuers are one per
  organization (`https://vstoken.dev.azure.com/<organization id>`) or Entra tenant
  (`https://login.microsoftonline.com/<tenant>/v2.0`), but every project, pipeline and app in it
  shares them, so an identity names one pipeline (`p://acme/shop/deploy`) or service connection
  (`sc://acme/shop/roll-prod`, or Entra's `/eid1/…` subject) exactly, without `*` (see "Azure
  Pipelines" above). Narrow further with claims
  (`ref=refs/heads/main`, or GitHub's `environment`). A token that lives longer than a day is
  refused. Key fetches are one at a time per issuer, and a failed one isn't retried for 30 s.
  Logins are capped at 120 a minute per identity and 240 per client address (429 `rate_limited`),
  counted per node before any work.
- **Claims.** Each of an identity's `claims` maps a claim to a value or a pattern with `*` (the
  subject's rules: `*` is any run of characters, nothing else is special, never `**` recursion).
  The key is a top-level claim's name (`ref`), or a JSON pointer (RFC 6901, starting with `/`)
  into nested claims: `{"/realm_access/roles": "deployer"}` for Keycloak's realm roles,
  `{"/groups": "team-*"}` for Entra ID's `groups` or `roles`, `{"/groups_direct": "acme/ops"}` for
  GitLab's. In a pointer `~1` stands for `/` and `~0` for `~`, and a decimal segment indexes an
  array (`/groups/0`). Through a pointer, an array claim matches when any of its first 256
  elements does (one level: an array inside an array never matches), and numbers and booleans
  match their JSON text (`2`, `true`). A plain name matches only a string claim, as rules made
  before pointers did, so an existing rule never starts matching a list or a number it used to
  refuse: write `/groups`, not `groups`, to match a list. An object, `null` or a missing claim
  never matches. A pointer has at most 8 segments,
  none empty, and can't start at `iss`, `aud`, `sub`, `exp`, `nbf` or `iat`. Claims only narrow
  the subject: a shared issuer's subject still names one tenant, whatever the claims say. The rules
  are `roll_oidc::check_identity` and `Jwt::unmatched`, which roll-access's trusted machine clients
  use too.
- **Pinned keys, for an issuer roll can't reach.** A cluster signs its service accounts' tokens as
  `https://kubernetes.default.svc.cluster.local` (k3s, kind, kubeadm, a private EKS endpoint), an
  issuer roll can't and shouldn't fetch. An identity can carry the issuer's public keys instead
  (`jwks`, a JWKS document up to 64 KiB with at least one RS256 or ES256 key; as Vault's
  `jwt_validation_pubkeys`): the issuer is then only compared with the token's `iss`, never
  fetched, and only those keys verify a login (`pinned_keys` counts them). When the cluster's keys
  change, replace them (`PUT …/identities/{id}/jwks`, `roll secrets identities keys ID --jwks
  FILE`): the id, and every pod's `ROLL_IDENTITY`, stays. During a rotation, give the old and the
  new keys in one JWKS. Replacing them is making the identity: it then acts only while whoever
  replaced them can sign in and reach the project, and the tokens its logins got so far are revoked
  (workloads log in again; with #56, `roll run --watch` does it by itself on the 401).

  ```sh
  kubectl get --raw /openid/v1/jwks > jwks.json
  roll secrets identities add pods --jwks jwks.json --audience roll --for-env production \
    --issuer "$(kubectl get --raw /.well-known/openid-configuration | jq -r .issuer)" \
    --subject system:serviceaccount:shop:web
  ```

  Checked on k3s 1.33: a pod running as `shop/web` with a projected token for audience `roll`
  (`ROLL_OIDC_TOKEN_FILE`) ran `roll run` with production's values, and the audit log names
  `system:serviceaccount:shop:web`.
- **roll's own issuer.** An identity for roll's own issuer (`<public URL>/v1/secrets/oidc`) takes
  roll apps' instance tokens: its subject is one `roll-apps:<this project>:<service>:<env>`, no
  pattern and no other project's, and its tokens are checked against roll's keys in process
  ("roll's tokens for other products", above).
- **Who makes them.** Identities are made by people (or the operator), never by a token or a
  login; a login can't create tokens either, so nothing it makes outlives it.
- **Login.** `POST /v1/secrets/login {identity, token}` (public: the token is the credential)
  checks the signature against the issuer's keys (RS256 or ES256; its discovery document's
  `jwks_uri`, fetched over the guarded client: HTTPS, public addresses, no redirects, bounded reads;
  cached an hour, fetched again early for a new key id, kept a day if the issuer is down), `iss`,
  `aud`, `exp` and `nbf` (60 s leeway), the subject and the claims; then roll-access mints a
  `workload` token (`Access::mint_workload`: the identity's scope and client addresses for its
  TTL, owned by whoever made the identity, refused once they can't sign in; 409 past 1,000 live
  per owner). The token is looked up on every request like any other; secrets remembers which
  login got it (`identity_logins`), so its requests are `identity:<id>` in the audit log, and the
  login itself is `identity.login` with the token's `sub` and its id. Deleting the identity
  revokes every token it gave out.
  An OIDC token can be exchanged more than once while it lives (as with Vault): each login is
  audited.
- **The CLI.** With `ROLL_IDENTITY=idn_…` and no `ROLL_TOKEN`, `roll secrets …` and `roll run` log
  in by themselves, with the token from `ROLL_OIDC_TOKEN`, `ROLL_OIDC_TOKEN_FILE` (a Kubernetes
  projected service account token), Azure Pipelines' own endpoint (`SYSTEM_ACCESSTOKEN` mapped
  into the step: "Azure Pipelines" above) or GitHub Actions' own endpoint (`permissions: id-token: write`;
  audience `ROLL_OIDC_AUDIENCE`, default the server's URL):

  ```sh
  roll secrets identities add deploy --github acme/web --claim ref=refs/heads/main --for-env production
  ```

  ```yaml
  permissions: { id-token: write, contents: read }
  env: { ROLL_API_URL: https://roll.example.com, ROLL_IDENTITY: idn_… }
  steps:
    - run: roll run --env production -- ./deploy.sh
  ```

  A login lasts its TTL (5 to 60 minutes). A long-running process (`roll run --watch` in a pod,
  `render --watch`) logs in again by itself a quarter of the way before its login ends (at least
  30 s before), reading `ROLL_OIDC_TOKEN_FILE` again, since Kubernetes rotates a projected token's
  file. That login runs in the background while requests go on with the current token; one that
  fails or takes over 30 s leaves the current token in use while it lasts (tried again every 15 s;
  its lifetime is measured on the process's own clock). A token that has ended, or that the server
  refuses (401), logs in again before the next request: a pod needs no stored secret, as with Vault's Kubernetes auth. The cluster's service
  account issuer has to be one roll can fetch keys from (EKS's and GKE's are public URLs):

  ```yaml
  # the pod: a projected token for roll's audience, and the identity to log in as
  env:
    - { name: ROLL_API_URL, value: https://roll.example.com }
    - { name: ROLL_IDENTITY, value: idn_… }
    - { name: ROLL_OIDC_TOKEN_FILE, value: /var/run/secrets/roll/token }
  volumeMounts: [{ name: roll-token, mountPath: /var/run/secrets/roll }]
  # volumes: [{ name: roll-token, projected: { sources: [{ serviceAccountToken: { audience: roll, path: token, expirationSeconds: 3600 } }] } }]
  ```
- **A CI job, end to end.** No stored secret: the job logs in with its own OIDC token as an
  identity limited to staging, then checks before it tests. Each step only sees staging, since
  check, scan and health answer what the caller may read:

  ```sh
  roll secrets identities add ci --github acme/web --claim ref=refs/heads/* --for-env staging
  ```

  ```yaml
  jobs:
    test:
      runs-on: ubuntu-latest
      permissions: { id-token: write, contents: read }
      env: { ROLL_API_URL: https://roll.example.com, ROLL_IDENTITY: idn_…, ROLL_ENV: staging }
      steps:
        - uses: actions/checkout@v4
        # roll on the runner's PATH; then:
        - run: roll secrets check .env.example   # every key the code reads is set in staging
        - run: roll secrets scan .               # no stored value was committed
        - run: roll secrets health               # nothing expired, missing, exposed or in the browser
        - run: roll run -- npm test              # the tests get staging's values, never on disk
  ```

### AWS IAM

A machine on AWS logs in with its own IAM role, as Vault's AWS auth (its IAM method) and
Infisical's AWS Auth do: an EC2 instance profile, an ECS task role, EKS's IRSA or Pod Identity, a
Lambda's role, or an IAM user's keys. Nothing is turned on in the account (unlike
`GetWebIdentityToken`, which needs IAM outbound federation), nothing is stored on the machine, and
roll holds no AWS credential.

- **How.** The client signs an `sts:GetCallerIdentity` request with the machine's credentials
  (Signature Version 4) but doesn't send it. It sends the signed request to roll
  (`POST /v1/secrets/login {identity, aws: {method, url, headers, body}}`), roll sends it to STS,
  and STS answers who signed it. roll then mints the same `workload` token an OIDC login gets
  (the identity's scope and addresses, its TTL, `identity.login` in the audit log with the ARN).
- **An identity** (`POST …/identities {name, kind: "aws", arn, environments?, paths?, actions,
  ttl_minutes?, allowed_ips?}`, `roll secrets identities add web --aws ARN`, or "AWS" in the
  dashboard's New identity) takes one role or user in one account:
  - `arn:aws:iam::123456789012:role/web`: any session of the role. That covers an instance
    profile's, a task's or a pod's. A role's path may be given; it isn't compared, since role
    names are unique in an account and a session's ARN drops the path.
  - `arn:aws:sts::123456789012:assumed-role/web/i-*`: only sessions whose name starts with
    `i-` (EC2 instances), or one session exactly without the `*`. A session name isn't a security
    boundary: whoever may call `sts:AssumeRole` on the role picks the session name, so anything
    that can assume `web` can name its session `i-…`. Only the role is the boundary; use a
    session prefix to tell your own workloads apart, never to keep someone out.
  - `arn:aws:iam::123456789012:user/ci`: one IAM user.

  A `*` anywhere but the end of a session name is refused, as is the account's root and a
  federated user. The ARN already names its account, so there's no separate `bound_account`.
- **What roll checks before it sends anything.** A client can't choose where roll sends the
  request:
  - The method is `POST`. The URL is exactly `https://sts.amazonaws.com/`,
    `https://sts.<region>.amazonaws.com/`, `https://sts-fips.<region>.amazonaws.com/` or
    `https://sts.cn-<…>.amazonaws.com.cn/`, with no path, query, port or user. roll rebuilds the
    URL from that host and checks it again before it sends.
  - The body is exactly `Action=GetCallerIdentity&Version=2011-06-15`.
  - The signature covers `Host`, `X-Amz-Date` and `X-Roll-Server-ID`, and nothing roll doesn't
    send. Only `Authorization`, `Content-Type`, `X-Amz-Date`, `X-Amz-Security-Token`,
    `X-Amz-Content-SHA256` and `X-Roll-Server-ID` go on to STS.
  - `X-Roll-Server-ID` names this server, as Vault's `iam_server_id_header_value` does. A request
    signed for another roll or a Vault can't be replayed here, so the ID has to be unique to the
    deployment. It's `ROLL_SECRETS_AWS_SERVER_ID` on rolld, or else its configured public URL
    (`ROLL_CODE_PUBLIC_URL`), or else 32 hex characters derived from the deployment key
    (`ROLL_SECRET_KEY`, HKDF under its own label; it reveals nothing of the key). Never a URL made
    up from the host and port: two deployments can share one. Clients send `ROLL_AWS_SERVER_ID`,
    or else their `ROLL_API_URL`, so **when the ID is derived, clients must set
    `ROLL_AWS_SERVER_ID`** to it (rotating the deployment key changes it; set
    `ROLL_SECRETS_AWS_SERVER_ID` to keep one across rotations). A wrong one is refused with the ID
    to use, and the identity shows it as `server_id` (the dashboard's and `roll secrets identities
    add`'s instructions include it when it isn't the API's URL).
  - `X-Amz-Date` is within 5 minutes of roll's clock.
  - `Authorization` is `AWS4-HMAC-SHA256` with exactly `Credential`, `SignedHeaders` and
    `Signature`, each once (a component named twice, in any case, is refused), and `Signature` is
    64 lowercase hex characters. The whole header goes to STS, so the signature roll keys a login on
    is the one STS checks: each signed request logs in once, and a replay with another `Signature=`
    in front of the real one is refused, not counted as new.

  roll then sends the request through rolld's guarded client: no redirects, a 15 s timeout, and
  at most 64 KiB of the answer read. STS's refusal (a bad signature, an expired session) is a 401
  with STS's error code. An assumed role's session ARN
  (`arn:aws:sts::123456789012:assumed-role/web/i-0abc…`) matches its role.
- **A signed request logs in once.** roll keeps a SHA-256 of each accepted request's signature
  (`aws_logins`, in the database every node shares, inserted with `ON CONFLICT DO NOTHING`) and
  refuses the same signature again with a 401, on any node. Rows go after 10 minutes, once the
  request's date is out of the ±5-minute window. A signed request is still a credential until it's
  used (STS takes it for 15 minutes, roll for 5). Clients send it to roll over HTTPS only. Logins share OIDC's rate limits
  (120 a minute per identity, 240 per client address).
- **The clients.** With `ROLL_IDENTITY=idn_…`, `roll run`, `roll secrets …`, the Go client
  (`FromEnv`, `LoginAWS`) and the Python client (`from_env`, `login_aws`) sign and log in by
  themselves when there's no OIDC token (`ROLL_OIDC_TOKEN`, `ROLL_OIDC_TOKEN_FILE`, GitHub
  Actions'). `ROLL_IDENTITY_KIND=aws` picks AWS anyway, for a GitHub Actions job with an AWS
  identity. Credentials come from the AWS SDKs' chain:
  1. `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` and `AWS_SESSION_TOKEN`.
  2. The `AWS_PROFILE` profile's static keys in `~/.aws/credentials` or `~/.aws/config`. For SSO,
     run `eval "$(aws configure export-credentials --format env)"` first.
  3. IRSA's `AWS_WEB_IDENTITY_TOKEN_FILE` and `AWS_ROLE_ARN`, exchanged with
     `AssumeRoleWithWebIdentity`.
  4. The container endpoint: ECS's `AWS_CONTAINER_CREDENTIALS_RELATIVE_URI`, or EKS Pod
     Identity's `AWS_CONTAINER_CREDENTIALS_FULL_URI` with its token file.
  5. IMDSv2 (unless `AWS_EC2_METADATA_DISABLED=true`).

  The region (`AWS_REGION`, `AWS_DEFAULT_REGION`, the profile's `region`) picks STS's regional
  endpoint; without one, the client uses the global endpoint. Each login reads the credentials
  again, so rotated instance or task credentials just work. The TypeScript client has no
  workload login, OIDC or AWS.

  ```sh
  roll secrets identities add web --aws arn:aws:iam::123456789012:role/web --for-env production
  ```

  ```sh
  # on the instance, task or pod: no secret to store
  ROLL_API_URL=https://roll.example.com ROLL_IDENTITY=idn_… roll run --env production -- ./server
  ```

- **Not yet checked against real AWS.** The signing matches AWS's published Signature Version 4
  example in all three clients, and roll's checks and STS's recorded answers are tested. A login
  through real STS from an instance, a task or a pod is still to be confirmed.

### Protected environments and approvals

An environment can be **protected** with a policy: `{"approvals": 1, "approvers": [principal…],
"notify_url"?}`. With `notify_url` (a Slack or Discord incoming webhook, or any HTTPS URL; kept
sealed, the environment shows only its host as `notify`), each request opened on it is announced:
the notice is written in the request's own transaction and delivered by the sweeper with retries,
as `{text, content, project, environment, request: {id, by, message, keys}, link}` (key names,
never values; Slack markup in `text` is escaped and Discord mentions are off, since the message is
the requester's). Notices aren't signed: an incoming webhook's URL is its own credential, and the
notice carries nothing a receiver should act on without opening the request in roll.
A write to a protected environment doesn't apply. It answers 409 `approval_required`, and the
client opens a **change request** with the same operations (`POST …/requests`). A request:

- holds its operations with values encrypted, and shows a diff without values; an approver who may
  reveal them sees each value now beside the one proposed (`…/requests/{id}/values`, audited);
- needs `approvals` approvals from approvers other than its author (an empty approver list means any
  principal with `admin`);
- applies as one change when approved (`…/apply`), and only if no key it touches changed since it
  was opened (otherwise it is `stale` and must be refreshed);
- can be applied without approvals by a caller with `admin` passing `{"bypass": true}`. The bypass
  is recorded in the audit log.

Rotation of a managed link and dynamic leases on a protected environment don't need approval: they
don't change what the secret is, only its credential.

**Change policies per folder**, as Infisical's approval policies on secret paths: a folder of an
environment that isn't protected as a whole can still take changes only through approved requests.
`PUT …/environments/{env}/folder-policies?path=/payments {approvals?, approvers?}` (admin;
`approvals` 1–10, 1 by default; 200 per environment, never on an ephemeral one; `roll secrets envs
protect staging --folder /payments [--approver …]`, `unprotect --folder`; the folder bar's "Require
approval" in the dashboard) covers the folder
and every folder under it; `GET` lists them and `DELETE ?path=` removes one. A write that touches a
covered key answers 409 `approval_required`, naming the folder, and the rest of the environment
takes writes as before. A request's **gates** are the environment's policy (when it's protected)
and every folder policy covering a key it changes; each needs its own approvals from its own
approvers (`gates: [{path?, approvals, approvers?, approved}]` on the request), a reviewer must be
an approver of at least one of them, and applying checks the gates as they are then (a policy added
after the approvals still has to be met). Every way to change a covered key goes through a request:
writes, deletes (deleting a covered folder, or one holding one, is refused), a promotion (it opens
a request), and setting a covered folder's imports needs `admin`, as on a protected environment. A
reviewer needs `admin` on each folder the request changes (a folder's own admin reviews its
folder's requests), and a review counts only for the gates whose scope its reviewer is admin of:
an admin of `/payments` approves for `/payments`' gate, never for a protected environment's own
(`gates` on each review); applying checks the gates in the applying transaction, and a bypass of gates
that aren't met is audited. Backups carry folder policies, and a restore sets them last. Only who
may change what is per folder: reading (untrusted readers, exposure in health) stays the
environment's.

### Audit log

Every read of values and every write is recorded: time, principal (`operator`, `token:tok_…`,
`app:…`), action, environment, path, keys, a summary and the client address (as rolld's gate saw
it, MCP's calls included; else the peer's, or with `ROLL_CODE_TRUST_FORWARDED_FOR` the last
`X-Forwarded-For` entry, the one the trusted proxy appended: earlier ones are the client's to
write). A value never is. A request that roll-access's gate let through because a person approved
it (a token's `ask`, approved once or remembered for a while) also names the approval: its id, who
approved it (in order: two people when a policy asks for two), and until when a remembered rule
allows it. The dashboard's audit log and an environment's
activity show it as "approved by …". This is how roll secrets does access requests (a time-boxed
`reveal` an approver grants, as Infisical's and 1Password's): a token that asks for `reveal`.
Listing metadata isn't audited. Revealing, exporting, resolving and every write are.
`GET …/audit?env=&key=&actor=&action=&before=&limit=&digests=` reads it newest first (`digests=false`
leaves out roll's hourly signed digests, as the dashboard's list does). Rows are kept for 400
days, and each project's rows are a hash chain roll signs hourly ("Tamper evidence" below).

- **Export**: `GET …/audit?format=csv` or `format=jsonl` (with the same filters, `since` too)
  answers up to 10,000 rows as a file.
- **Streaming to a SIEM**: an audit sink (`POST …/audit/sinks {url, from_start?, format?, token?}`, `admin`) is a
  public HTTPS URL that receives the project's new rows in order, in batches of up to 200:
  `{sink, project, events: AuditEntry[]}`, signed like syncs and every roll webhook (`X-Roll-Sink`, `X-Roll-Timestamp`,
  `X-Roll-Signature-256: sha256=HMAC(secret, "ts.body")`, `X-Roll-Delivery`; the `whsec_…` secret is shown once and kept
  sealed). Each sink keeps a cursor; the sweeper claims a sink in a write before delivering, so no
  two nodes send one batch, and a failed batch is sent again 5 minutes later. Deliveries go through
  the same `PublicOnly` client as syncs. At most 10 sinks a project.
- **In a SIEM's own format**, as Doppler's and Infisical's log streams offer: `format` `splunk`
  (HTTP Event Collector: one event per line, `time` in seconds, `sourcetype` `roll:audit`,
  `Authorization: Splunk <token>`), `datadog` (the Logs intake: a JSON array of logs, `message` the
  summary, `ddtags` the project and action, `DD-API-KEY`) or `bearer` (roll's JSON with
  `Authorization: Bearer <token>`, for other collectors). The `token` is sealed with the project's
  key and never shown again (`roll secrets sinks add URL --format splunk` reads it from stdin);
  such batches carry it instead of roll's signature, and each event names its project.
- **Into a roll bucket**: a sink whose URL is `bucket:<name>`, a bucket on the project's own canvas
  (any other is refused, and a bucket moved off the canvas later gets nothing more: it's checked at
every delivery), writes each batch as an object `roll-secrets/audit/<sink>/<delivery>.json`
  there, through roll storage in process: a compliance archive with the bucket's own lifecycle,
  versioning and Object Lock. A retried batch is the same delivery, so it writes the same object
  again, never a second one.
- **Into roll search**: a sink whose URL is `search:<cluster id>/<index>`, a cluster on the
  project's canvas (checked the same way), indexes each event as a document through the cluster's
  bulk route in process, its id the audit id, so a retried batch writes the same documents again;
  each names its project. Search and dashboards over the audit log with roll search's own
  Kibana-compatible API. A per-document error (a mapping that refuses a field) fails the batch,
  which is sent again later.
- **Into a roll stream**: a sink `stream:<stream id>/<topic>` (a stream on the canvas, checked at
  every delivery like a bucket) produces each audit event to that topic as one record, keyed by its
  audit `id`, through roll streams in process: consumers react as it's written. A retried batch can
  produce an event again, so a consumer dedupes by key.

### Tamper evidence

Audit rows sit in the same database as everything else, so someone who can write it could edit or
delete them. As CloudTrail's log file integrity validation does, each project's log is a hash chain
with signed digests, so such a change shows:

- **The chain.** Each row has a `seq` (1, 2, 3… per project) and `hash = sha256(previous hash,
  row)`, written in the row's own transaction. The row's bytes are a length-prefixed encoding of
  every field the API shows but `id` (`crates/roll-secrets-chain`, `canonical`), so they hash the same from SQLite,
  Postgres or an export. The project's head (`audit_heads`) is updated in that transaction too:
  writers queue on the head's row lock (or SQLite's one writer), and a unique index on
  `(project_id, seq)` refuses a second row at one number anyway, so racing writers, on one node or
  several, never fork the chain.
- **Under load.** Every read of values writes a row, so 500 pods of a rollout reading one project
  take its head 500 times. Reads waiting on one node are group-committed: one of them writes
  everyone's rows (up to 256) onto the chain in one read-committed transaction, in the order they
  came, and the others answer once it commits. Batches from several nodes queue on the chain's
  claim (`code_sql::Conn::claim`, an advisory lock) rather than fail and re-run. A change writes its
  own row in its own serializable transaction, as before; when a batch commits under it, its next
  attempt holds the claim from before its snapshot, so reads can't starve it. Measured with
  `examples/bench concurrent` (Postgres 18 in Docker, `max_connections=500`, one project of 20
  keys, a writer changing a key every 20 ms; medians of 2 alternating runs on one node, 3 on eight,
  each pool its own; a shared 36-core host at load 35–50, so the numbers are low and noisy:
  base varied 2x run to run), reads a second and p99:

  | resolvers | before the chain | the chain alone (#281) | batched and claimed (now) |
  | --- | --- | --- | --- |
  | 1 node, 1 | 300, 6 ms | 236, 14 ms | 184, 20 ms |
  | 10 | 2,622, 28 ms | 420, 521 ms | 914, 77 ms |
  | 50 | 5,002, 42 ms | 704, 881 ms | 2,626, 52 ms |
  | 200 | 4,792, 103 ms | 468, 2.7 s (104 reads unaudited) | 3,029, 162 ms |
  | 500 | 4,436, 229 ms | 428, 4.4 s (515 unaudited) | 3,034, 332 ms |
  | 8 nodes, 10 | 1,493, 44 ms | 515, 345 ms | 590, 112 ms |
  | 50 | 1,999, 80 ms | 469, 1.5 s | 1,458, 93 ms |
  | 200 | 2,269, 198 ms | 181, 3.5 s (180 unaudited) | 1,994, 313 ms |
  | 500 | 2,419, 317 ms | 343, 3.5 s (706 unaudited) | 2,468, 514 ms |

  The chain alone re-ran each read's write up to 40 times (70,000 serialization retries in 10 s at
  500), gave up on some (the read answered, unaudited), and failed most changes made meanwhile: 3–5
  changes in 10 s, 2–3 of them failing, against 23–378 before the chain. Now no read goes
  unaudited and no change fails (52–300 in 10 s, each retried once: the only retries left). What's
  left is the chain itself: one batch at a time per project, each holding the claim for about three
  round trips and a commit. At 50 resolvers or more that's within 2x of before the chain on every
  line; few readers per node make small batches (eight nodes sharing ten readers: 0.4x). of `{project, seq, hash, start}` under a key
  derived from `ROLL_SECRET_KEY` (HKDF, its own label `audit:head:v1`), checked before every append
  and remade after it (two HMACs a write). Plain SHA-256 lets a database writer recompute every
  hash; the MAC means they can't move the head to match: not back over rows they cut from the end,
  not forward over rows they rewrote after the last digest, and not where the log starts. A head
  that fails the check is never MACed again or signed: it stays broken (`head`) until someone looks.
  After a deployment-key rotation the previous key's MACs still pass, and the hourly digests remake
  every head's with the current one.
- **Digests.** Every hour, the sweeper appends an `audit.digest` row (actor `roll:audit`) to each
  project's chain, moved or not. Its `digest` is a JWS (EdDSA, `typ` `roll-audit-digest`) signed
  with roll's digest key (below): `{project, seq, hash, last_at, at, start}`, the row before it and
  where the log starts. The digest is claimed per project in its own write, so two nodes don't sign
  one twice. Sinks get digests like any other row, so a SIEM or a locked bucket holds roll's
  signatures outside the database: recomputing every hash after an edit no longer matches what
  roll signed.
- **Gaps.** Before signing, the sweeper reads the newest digest the chain itself holds (with none,
  its first row). When that's over two hours old, or isn't the digest the head says roll last
  wrote, the rows after it can't be vouched for: a head put back from a copy passes its MAC, since
  nothing in the database can't be put back with it. The new digest then carries
  `gap: {after, since}` (that digest's seq, or 0, and its time), and `verify` reports the `gap` for
  as long as the digest is in the log, after any other break, so an old gap never hides a newer one.
  The head's own `digest_seq`/`digest_at` aren't MACed: they're only the sweeper's schedule, and a
  MAC over them would pass on a copy too. A signer stopped for over two hours (every node down)
  shows as a gap the same way: roll can't tell the two apart.
- **Verifying.** `GET …/audit/verify?since=` (`read`) walks the chain from its start, or from the
  last digest before `since`, and answers `AuditVerification`: the rows walked (`from`–`head`), how
  far a valid digest vouches for them (`through`, `signed_at`), and the first thing that doesn't
  hold (`broken: {seq, reason, detail}`): `edited` (a row doesn't match its hash), `missing` (a
  gap), `rewritten` (the chain isn't what a digest signed), `signature` (a digest no key of roll's
  signed, or an issuer key's after the chain's first digest-key digest), `pulled` (a digest on a
  pulled key that no later good digest covers: below), `truncated` (rows gone from the end; the
  head still counts them), `cut` (rows gone from
  the start other than by retention), `unchained` (a row added outside the chain), `head` (the
  head doesn't carry roll's MAC), `stale` (the newest digest is over two hours old: the signer
  stopped, or rows after it were cut and the head put back from a copy) or `gap` (a digest roll
  signed after such a span). The dashboard's audit log
  shows "Verified through #N" or the break. `roll secrets audit verify [--since TIME]` asks the
  same. Two more fields say what the digests rest on: `legacy: {signer: "issuer key", digests,
  through}` (digests from before the digest key, which count) and `pulled: [{kid, pulled_at,
  digests, first, last, countersigned}]` (digests on pulled keys, which don't); both are left out
  when there are none.
- **The digest key.** Digests have a key of their own, as CloudTrail's digest files do, never
  roll's issuer key (which clouds and apps' tokens trust): a leak of either forges nothing the other
  signs. Ed25519 (JWS `EdDSA`): only roll's code checks a digest (the server and `audit verify`),
  so nothing needs RS256 as Azure does; the keys are small and quick to make, signatures are
  deterministic, and a digest key can't pass for an issuer key or the other way round (each kind is
  checked with its own algorithm only). A row in `audit_keys` holds the private key sealed under a
  data key of its own (sealed with `ROLL_SECRET_KEY`), with its kid and its `legacy` list as
  associated data, and is roll's only when that opens and is the private half of the row's `x`.
  Health's `resealed` counts digest keys after a deployment-key rotation, as it does projects'.
  Its public halves are `GET /v1/secrets/audit/jwks` (public: `keys`, every digest key but pulled
  ones, retired included, as their digests stay in the log; `legacy`, the issuer keys whose digests
  still count; `pulled: [{kid, pulled_at}]`). It's made at startup, before the issuer's key.
- **Its rotation.** Every 90 days (`ROLL_SECRETS_AUDIT_KEY_ROTATE_DAYS`, at least 1) the sweeper
  makes a new key that signs at once (nothing caches the digest JWKS the way clouds cache the
  issuer's), and the old one retires: it signs no more and stays published. `POST
  /v1/secrets/audit/key/rotate` (`roll secrets audit key rotate`; the operator's) rotates now.
  Save the digest JWKS again after any rotation.
- **Pulled digest keys.** For a key that may have leaked, `{"now": true, "retire": "<kid>"}`
  (`roll secrets audit key rotate --now --retire KID`; the operator's, both or neither; a kid that
  isn't one of roll's, or was pulled already, is a 404) makes a new key that signs at once and pulls
  `<kid>` in the same write: its private half is wiped (so no database edit makes it roll's again)
  and it moves to the JWKS's `pulled`. Every project's head is made due, and the rotation signs
  every chain under the new key before it answers (`{kid, pulled, signed}`). Each is an
  `audit.key.rotate` row (actor `operator`) in every project's log, either kind of rotation. Then:
  a digest on a pulled key never vouches for anything itself, whatever time it claims (an attacker
  chooses that too). It's reported in `pulled`; the chain walks through it, and the next digest
  under a good key covers it (its hash runs through every row before it, and roll signs it only
  over a head carrying its MAC), which is `countersigned`. One no later good digest covers is a
  `pulled` break: the log from there on needs the next good digest, or a sink's copy, to be
  trusted. An export taken before the rotation is such a log: export it again.
- **What a leaked digest key can do.** Sign any digest, for any project, rows and start. Against
  the server, nothing more: rows it rewrote change the chain's last hash, and the head that would
  have to match carries a MAC under `ROLL_SECRET_KEY`, so `verify` says `rewritten` or `head`
  whatever digests it signs. Against an export checked offline (no head, no MAC), it can forge a
  whole log that verifies, until it's pulled; after, every digest it signed is refused unless a
  digest under a later key covers it, which only roll can sign. It forges no cloud's or app's
  token, and pulling it touches no cloud trust. A saved digest JWKS from before the pull still
  trusts the key: save it again after an emergency.
- **Legacy digests.** Digests an older roll signed with its issuer key (RS256) keep verifying,
  marked `legacy: issuer key`, against the issuer keys in the digest JWKS's `legacy` (or an issuer
  JWKS saved before the upgrade: `--jwks` again), and only before the chain's first digest under
  the digest key: from the first hour after the upgrade, new digests use the digest key, and the
  issuer key never signs one again. A pulled issuer key's legacy digests are treated as a pulled
  digest key's.
- **Whose keys.** A row in `oidc_keys` or `audit_keys` is roll's only when its sealed private key
  opens with the deployment key and is the private half of the row's public key. Only those sign,
  check digests and are published (the issuer's in its JWKS, which GCP, Azure and AWS trust too).
  A row that fails is logged by kid (never the key) and ignored, so a database writer who adds a key
  of their own gets neither a digest nor a cloud to accept their signature.
- **Offline.** `roll secrets audit verify -p prj_… --file audit.jsonl --jwks keys.json` checks an
  export (`audit --format jsonl`, unfiltered: a filtered one has gaps) on the machine it runs on.
  `--jwks` is required: the digest JWKS as saved earlier from `/v1/secrets/audit/jwks` and kept
  apart from roll, since keys fetched from the server being checked would vouch for themselves.
  Save it once, and again after each rotation of the digest key (an emergency's names the pulled
  key). It carries the legacy issuer keys too; an issuer JWKS saved before the upgrade can be given
  as a second `--jwks` (its keys count only where the digest JWKS names them). A file whose digests name another project than `-p`, or more than one, is refused (a A file whose digests name another project than `-p`, or more than one, is refused (a
  `prj_…` id needs no server; a name is looked up). An export of the newest 10,000 rows starts
  checking at its first digest, and a row without a `seq` after the chain began is `unchained`.
  Staleness isn't checked offline: an export is a copy from some time.
- **Retention** cuts the chain only at a digest at least 400 days old, which is then where the log
  starts: a signed anchor, its hash the one before it. The digest written in the cut's own
  transaction signs the new start, so rows removed from the start any other way are a `cut`.
- **Older rows** written before the chain have no `seq` or hash: they count as `unchained`, the
  chain starts at the first row after them, and they're dropped by age as before. A row written by
  a node still running an older roll during an upgrade shows as `unchained` too.
- **What it can't show without a sink**: someone holding both the database and `ROLL_SECRET_KEY`
  can rewrite anything and sign it again, MAC and digests alike. Someone who can write only the
  database can put back a copy of its rows and head from earlier: once that copy's newest digest is
  over two hours old, roll's next digest records the gap for good, but rows written in the two hours
  after it (digests included) can be cut and the copy put back before then, and roll signs on
  without a trace in the database. Only a copy held outside it catches that: a sink (a SIEM, a
  locked bucket) that already received the digests the copy lacks, or a saved `--jwks` file for a
  key roll never made. Someone holding a digest key but not `ROLL_SECRET_KEY` is above ("What a
  leaked digest key can do").


## HTTP API

Bodies are JSON and every error is `{"error": string, "code"?: string}`. Paths below are under
`/v1/secrets/projects/{project}`. `{env}` is an environment slug, `{key}` a secret key, and the folder
is the `path` query parameter (default `/`). `{project}` is a workspace project id; a project that
isn't in the workspace is 404.

| Route | Body | Answer |
| --- | --- | --- |
| `GET /v1/secrets/health` | | `{ok, key: "env"\|"kms"\|"file"\|"shares"\|"missing", resealed: "all"\|number\|null, customer_managed, issuer_pulled: {kid, pulled_at, detail}[], links: string[], syncs: string[]}` (`links`, `syncs`: the kinds registered; `issuer_pulled`: issuer keys an emergency rotation pulled in the last 30 days) |
| `GET`, `POST /v1/secrets/unseal` | `{share}` (`POST`) | `{sealed, have?, need?, error?}`: a sealed node's progress (public; see "Starting sealed"); 429 with `Retry-After` past the limits |
| `GET` | | `{project, environments: Environment[], previews: number, history_days: number \| null}` |
| `PATCH` | `{history_days}` | `{history_days}` (`admin`) |
| `GET /grid?path=` | | `Grid` |
| `GET /environments` | | `Environment[]` in order |
| `POST /environments` | `NewEnvironment` | 201 `Environment` |
| `DELETE /environments?label=k=v` | | `{deleted: string[]}` (ephemeral only) |
| `PUT /environments/order` | `{order: slug[]}` | `Environment[]` |
| `GET /environments/{env}` | | `Environment` |
| `PUT /environments/{env}` | `Ensure` | 201 or 200 `{environment, created, change}` (409 `mismatch`; `branches` on a standard environment is a 400, an addition by anyone but roll previews or the operator a 403) |
| `PATCH /environments/{env}` | `{slug?, name?, expires_at?, max_age_days?}` | `Environment` |
| `DELETE /environments/{env}?confirm=` | | 204 |
| `PUT /environments/{env}/policy` | `{protected, approvals?, approvers?, notify_url?}` | `Environment` |
| `GET /environments/{env}/folder-policies` | | `FolderPolicy[]` |
| `PUT /environments/{env}/folder-policies?path=` | `{approvals?, approvers?}` | `FolderPolicy` (`admin`) |
| `DELETE /environments/{env}/folder-policies?path=` | | 204; 404 when the folder has none (`admin`) |
| `GET /environments/{env}/folders?path=` | | `{path, folders: Folder[]}` |
| `POST /environments/{env}/folders` | `{path}` | 201 `Folder` |
| `DELETE /environments/{env}/folders?path=` | | `{change: Change \| null}` |
| `GET /environments/{env}/imports?path=` | | `Import[]` |
| `PUT /environments/{env}/imports?path=` | `Import[]` | `Import[]` |
| `GET /environments/{env}/exports?path=` | | `Export[]` |
| `POST /backup` | `{recipient}` | `{sealed, context}`: the whole project, sealed to `recipient` (admin) |
| `GET /backup/schedule` | | `BackupSchedule` or null (admin): bucket, recipient, `every_hours`, `keep`, `next_at`, `last_at`, `last_key`, `last_error` |
| `PUT /backup/schedule` | `{bucket, recipient, every_hours?, keep?}` | `BackupSchedule` (admin; one of the project's roll buckets; the first backup at once) |
| `DELETE /backup/schedule` | | 204 (admin; the backups made stay) |
| `POST /restore` | `Archive` (an opened backup) | `{environments, secrets, warnings}` (admin; an empty project) |
| `GET /suggestions` | | `Suggestion[]`: canvas resources linked nowhere yet, with their usual keys and where they'd go |
| `GET /shared` | | `Import[]`: the folders other projects share with this one |
| `PUT /environments/{env}/exports?path=` | `{project}[]` | `Export[]` (admin) |
| `GET /environments/{env}/secrets?path=` | | `Secret[]` (own, imported, inherited; no values) |
| `GET /environments/{env}/secrets/{key}?path=&type=` | | `Secret` |
| `PUT /environments/{env}/secrets/{key}?path=` | `SetSecret` | `Written`; 409 `stale` when `if_version` isn't the key's version |
| `DELETE /environments/{env}/secrets/{key}?path=&type=&if_version=` | | `Written`; 409 `stale` when `if_version` isn't the key's version |
| `GET /environments/{env}/secrets/{key}/value?path=&resolve=` | | `{value, problems}` |
| `POST /environments/{env}/decoys` | `NewDecoy` | 201 `Secret` (admin; a new key, never its value; 409 `exists` when the key is there; "Decoys") |
| `GET /environments/{env}/secrets/{key}/totp?path=` | | `{code, expires_at, period}` (`reveal`; a key that isn't a TOTP key is a 400; audited as `secrets.totp`, without the code) |
| `GET /environments/{env}/secrets/{key}/versions?path=&type=` | | `Version[]` newest first |
| `GET /environments/{env}/secrets/{key}/versions/{v}/value?path=&type=` | | `{value}` |
| `POST /environments/{env}/secrets/{key}/rotate?path=` | | `Applied` |
| `POST /environments/{env}/secrets/{key}/purge?path=` | `{before_version?}` | `{purged}` (`admin`) |
| `GET /environments/{env}/values?path=&format=&resolve=&dynamic=&strict=&digest=&codes=&name=&namespace=&seq=&at=` | | `Values` (`codes=1`: `<KEY>_CODE` beside each TOTP key, never a 304; `dynamic=false`: a dynamic key is left out, named in `dynamic`, instead of a fresh credential; `strict=1`: any problem is a 409 `unresolved` naming the keys, so a reader keeps what it had; `digest=1`: the folder's `…/digest`, taken just before the read, as `digest`; with `seq`: as stored after that change, `seq` in the answer), or text with `format`: `dotenv`, `shell`, `json`, `yaml`, `docker` (`--env-file`; a value with a line break is a 400 naming the key) or `kubernetes` (a Secret named `name`, default the environment, in `namespace`); with `digest=1` or `If-None-Match` an `ETag` (the digest), and `If-None-Match` with the digest still current is a 304 that decrypts and issues nothing, and writes one `secrets.revalidate` row a day per caller and folder (so health and insights still see the folder read) (never for a folder holding a dynamic credential unless `dynamic=false`: a lease ends without moving the digest) |
| `GET /environments/{env}/insights?path=` | | `Insights` (read) |
| `GET /environments/{env}/reach?path=&key=` | | `Reach` (a project admin's): tokens and identities that reach it, syncs, live leases and attributed shares, projects it's shared with, folders that import it, readers in 30 days, `frozen` |
| `POST /environments/{env}/reach/cut-off?path=&key=` | `{why, tokens?, identities?, leases?, shares?, syncs?, freeze?}` | `CutOff`: what was cut, what was left to the operator (`skipped`: tokens reaching beyond the project) and what failed, with why (a project admin's; idempotent) |
| `DELETE /environments/{env}/reach/cut-off?path=` | | 204: the folder's freeze lifted (a project admin's; 404 when it isn't frozen) |
| `GET /environments/{env}/digest?path=&since=&wait=` | | `{digest}` (read: changes when the values would; a live link counts its value's keyed fingerprint as the product answers, read at most every 10 s, and at once when roll rotates it or the product's own password, reset or restore route changes it). With `since` (a digest the caller has) and `wait` (seconds, at most 25), a long poll: it answers as soon as the digest differs, else with the same one when `wait` runs out (at most 32 held per principal and 4096 per node; past that it answers at once); `roll run --watch`, `roll secrets render --watch` and the TS, Go and Python watches use it (and roll apps' redeploys in process, through `SecretSource::wait_digest`: one held poll per environment), so a change to the project's own values reaches them in a second or two (a live link's product-side change, or another project's shared folder, within about 10 s) |
| `POST /environments/{env}/resolve` | `{path?, consumer, seal_to?, untrusted?, dynamic?, codes?}` | `Values` or `{sealed, context, problems, leases, dynamic, linked, followed}` (`dynamic: false`: a consumer that can't renew leases, such as a preview's service, gets no dynamic credential; those keys are left out, named in `dynamic`; a key linked to a resource the ephemeral environment branched is left out and named in `followed`, in the clear beside a sealed box: names only) |
| `GET /environments/{env}/changes?before=&limit=` | | `Change[]` newest first |
| `POST /environments/{env}/changes` | `Batch` | 201 `Applied` |
| `POST /environments/{env}/diff` | `{dotenv} \| {json}, path?` | `{added, changed, same, missing, references, linked}`: a local copy against the folder by fingerprint, names only (`reveal`, audited) |
| `POST /environments/{env}/import` | `{dotenv \| json, path?, overwrite?, replace?, message?, request?, if_seq?}` (`json`'s array items may say `sensitive: true`: the key gets the tag) | 201 `Applied & {skipped}` or `{request, skipped}` |
| `GET /environments/{env}/changes/{seq}?state=` | | `ChangeDetail` |
| `POST /environments/{env}/rollback` | `{to?: seq, at?: ms, message?, bypass?}` | 201 `Applied` |
| `GET /compare?from=&to=&path=` | | `Compare` |
| `POST /environments/{env}/promote?from=` | `{path?, keys?, message?}` | 201 `Applied` |
| `GET /environments/{env}/requests?state=` | | `Request[]` |
| `POST /environments/{env}/requests` | `Batch` | 201 `Request` |
| `GET /environments/{env}/requests/{id}` | | `Request` |
| `GET /environments/{env}/requests/{id}/values` | | `RequestValue[]` (`reveal`; audited: the value now and the one proposed) |
| `POST /environments/{env}/requests/{id}/review` | `{verdict: "approve"\|"reject", comment?}` | `Request` |
| `POST /environments/{env}/requests/{id}/apply` | `{bypass?}` | `Request` |
| `POST /environments/{env}/requests/{id}/close` | | `Request` |
| `GET /links` | | `LinkTarget[]` |
| `GET /leases` | | `Lease[]` |
| `DELETE /leases/{id}` | | 204 (whoever may delete there, or the caller it was issued to while it may still reveal there; 403 for another's) |
| `POST /leases/{id}/renew` | `{ttl_minutes?}` | `Lease` (as ending one: `delete` there, or the caller it was issued to with `reveal`) |
| `GET /identities` | | `Identity[]` (admin) |
| `POST /identities` | `NewIdentity` | 201 `Identity` |
| `DELETE /identities/{id}` | | 204 (its tokens revoked) |
| `PATCH /identities/{id}` | `{disabled}` | `Identity` (admin; a person's): `false` enables one a cut-off disabled, `true` disables it and revokes its live logins |
| `PUT /identities/{id}/jwks` | `{jwks}` | `Identity` (admin; a person's): its pinned keys replaced, its id kept (400 for an `aws` one) |
| `POST /v1/secrets/login` | `{identity, token}` or `{identity, aws: {method, url, headers, body}}` | `{token, expires_at, project, identity}` (public; `aws`: a signed `GetCallerIdentity`, "AWS IAM") |
| `GET /audit?env=&key=&actor=&action=&before=&since=&limit=&digests=&format=` | | `AuditEntry[]` newest first, or CSV/JSONL with `format`; `digests=false` leaves out `audit.digest` rows |
| `GET /required` | | `Required[]` |
| `PUT /required` | `Required[]` | `Required[]` (replaces the list; `admin`) |
| `GET /shares` | | `Share[]` newest first |
| `POST /shares` | `{ciphertext, note?, views?, ttl_minutes?, from?: {environment, path?, key}}` | 201 `Share` (`from`: the key the value came from, `reveal` there) |
| `DELETE /shares/{id}` | | 204 |
| `GET /syncs` | | `{syncs: Sync[], kinds: SyncKind[]}` |
| `POST /syncs` | `NewSync` | 201 `Sync & {signing_secret?}` (shown once; `admin`) |
| `POST /syncs/preview` | `NewSync` | `{names, credentials, missing_credentials, unknown_keys}`: what it would push, before it exists (`admin`; names only, the platform isn't called) |
| `DELETE /syncs/{id}` | | 204 (`admin`) |
| `PATCH /syncs/{id}` | `{paused}` | `Sync`: pause or resume, with what running the kind needs (`admin`; `reveal` for a product's kind) or `admin`. Resuming one a cut-off paused is `admin` only; it's due at once |
| `POST /syncs/{id}/run` | | `Sync` after pushing every key (`admin`; 409 `running` while another push holds it) |
| `GET /v1/secrets/shares/{id}` (public) | | `{note, views_left, expires_at}` |
| `POST /v1/secrets/shares/{id}/open` (public) | | `{ciphertext}` (one open used) |
| `POST /environments/{env}/value-requests?path=` | `NewValueRequest` | 201 `ValueRequest` with its `token` (`rlq_…`, only here) and `link` (`write` on the folder) |
| `GET /value-requests` | | `ValueRequest[]` newest first (never a key or a token) |
| `DELETE /value-requests/{id}` | | 204 (whoever asked, or an admin of the folder) |
| `GET /v1/secrets/value-requests/{token}` (public) | | `{id, note, keys, expires_at, public_key}`; 404 for anything but a live token; 410 `gone` once answered, expired or cancelled |
| `POST /v1/secrets/value-requests/{token}/answer` (public) | `{values: Record<string, Sealed>}` | 204; 410 `gone`; 429 past 5 a minute per request or 10 per address |
| `GET /audit/verify?since=` | | `AuditVerification`: the chain walked from its start (or the last digest before `since`) and the first break; `legacy` (digests the issuer key signed before the digest key) and `pulled` (digests on pulled keys, and the good digest covering each) when there are any |
| `GET /audit/sinks` | | `Sink[]` (`admin`) |
| `POST /audit/sinks` | `{url, from_start?, format?, token?}` (an HTTPS endpoint, or on the canvas `bucket:<name>`, `search:<cluster>/<index>` or `stream:<id>/<topic>`; `format` `splunk`, `datadog` or `bearer` takes an HTTPS endpoint and its `token`, sealed) | 201 `Sink & {secret}` (shown once; a `Sink` names its `format`, never its token) |
| `DELETE /audit/sinks/{id}` | | 204 |
| `GET /v1/secrets/findings` | | `{project, project_name, findings: Finding[]}[]`: every project's findings the caller may read, projects with none left out |
| `GET /health?code=` | | `Finding[]`, worst first: every standard environment's findings the caller may read (`read` on the project) |
| `POST /leaks` | `{candidates: string[]}` (at most 20,000); header `X-Roll-Guard: 1` from a credential guard (roll MCP's check of what a tool call would write), audited as `secrets.guard` instead of `secrets.scan`; rolld keeps the header only on a request roll MCP's guard tool makes in process, and removes anyone else's | `{index, key, environment, path}[]`: candidates that are values the caller may reveal (`read` on the project) |
| `GET /kms` | | `{key: string \| null}`: the customer-managed key wrapping the project's data key (admin) |
| `PUT /kms` | `{key}` (a KMS key's ARN) | `{key}` after encrypting and decrypting with it (admin; 400 when roll can't use it) |
| `DELETE /kms` | | `{key: null}`: the data key sealed by roll's deployment key alone again (admin) |
| `GET /aws-roles` | | `AwsRole[]` (`read` on the project) |
| `POST /aws-roles` | `{name, role_arn}` | 201 `AwsRole` after assuming it (admin; 400 when its trust policy doesn't pin `sts:ExternalId`) |
| `DELETE /aws-roles/{id}` | | 204 (admin; 409 `in_use` while a key links to it) |
| `PATCH /aws-roles/{id}` | `{name?, role_arn?, trust?}` | `AwsRole` (admin; a new ARN or trust is proved as a new role's is, before anything changes; 409 `in_use` switching a role IAM users rotate through away from `external_id`; leases already issued run out) |
| `GET /aws-users` | | `AwsUser[]` (`read` on the project) |
| `POST /aws-users` | `{name, role, user}` | 201 `AwsUser` (`admin`; lists the user's keys as the role first: 400 with AWS's code) |
| `DELETE /aws-users/{id}` | | 204; 409 while a key links to it or a retired key waits (`admin`) |
| `PATCH /aws-users/{id}` | `{name?, role?}` | `AwsUser` (admin; another ExternalId role of the project, proved by listing the user's keys through it; keys already issued keep working) |
| `GET /rotations` | | `Rotation[]`: every managed credential the caller may read (environment, keys, resource, `rotate_days`, `rotate_window`, `issued_at`, `next_due`: with a window, its next opening once due, `last_error`) |
| `GET /rotators` | | `Rotator[]` (`read` on the project) |
| `POST /rotators` | `{name, url}` | 201 `Rotator & {secret}` (`admin`; the signing secret shown once) |
| `DELETE /rotators/{id}` | | 204; 409 while a key links to it (`admin`) |
| `PATCH /rotators/{id}` | `{name?, url?}` | `Rotator` (`admin`; a new URL is checked first) |
| `POST /rotators/{id}/rotate-secret` | | `Rotator & {secret}` (`admin`; the new signing secret, shown once) |
| `GET /saas` | | `SaasRotator[]` (`read` on the project; never the credential) |
| `POST /saas` | `{name, vendor, target, credential}` | 201 `SaasRotator` (`admin`; never the credential) |
| `DELETE /saas/{id}` | | 204; 409 while a key links to it or a retired key waits (`admin`) |
| `GET /outside-databases` | | `OutsideDatabase[]` (`read` on the project; never the URL) |
| `POST /outside-databases` | `{name, url, roles?: [a, b], group?, ca?, gateway?}` | 201 `OutsideDatabase` (`admin`; connects and checks the roles first: 400 with the server's reason) |
| `DELETE /outside-databases/{id}` | | 204; 409 while a key links to it or a retired password waits (`admin`) |
| `PATCH /outside-databases/{id}` | `{name?, url?, ca?}` (`ca: ""` clears it) | `OutsideDatabase` (`admin`; a new URL is checked first: 400 with the server's reason) |
| `POST /outside-databases/{id}/rotate-admin` | | `OutsideDatabase` (`admin`; 400 when the server refuses, 409 while another rotation runs, 502 when the new password was kept but doesn't log in) |
| `GET /directories` | | `Directory[]` (`read` on the project; never the bind password) |
| `POST /directories` | `{name, url, bind_dn, bind_password, flavor: "openldap" \| "active_directory", user_base_dn?, managed_accounts?: [dn, dn?], groups?: [dn], ca?, gateway?}` | 201 `Directory` (`admin`; binds once and finds the accounts, groups and base first: 400 with the server's reason) |
| `DELETE /directories/{id}` | | 204; 409 while a key links to it, or a retired password or a dynamic user waits (`admin`) |
| `GET /snowflake-accounts` | | `SnowflakeAccount[]` (`read` on the project; never the admin key, only its fingerprint) |
| `POST /snowflake-accounts` | `{name, account, privatelink?, admin_user, admin_role?, admin_private_key, auth?: "key_pair" \| "password", managed_user?, roles?: [role]}` | 201 `SnowflakeAccount` (`admin`; signs in once and finds the managed user and the roles first: 400 with Snowflake's reason) |
| `DELETE /snowflake-accounts/{id}` | | 204; 409 while a key links to it, or a retired key or a dynamic user waits (`admin`) |
| `GET /atlas-projects` | | `AtlasProject[]` (`read` on the project; the client id, never the secret) |
| `POST /atlas-projects` | `{name, group_id, client_id, client_secret, managed_users?: [a, b], roles?: ["role@db"]}` | 201 `AtlasProject` (`admin`; takes a token and finds the managed users first: 400 with Atlas's status and error code) |
| `DELETE /atlas-projects/{id}` | | 204; 409 while a key links to it, or a retired password or a dynamic user waits (`admin`) |
| `GET /gateways` | | `Gateway[]` (`read` on the project), each with its link's status |
| `POST /gateways` | `{name, token?: false}` | 201 `{gateway: Gateway, token?: "rlt_…"}` (`admin`; the token is shown once, and made only for a person or the operator) |
| `DELETE /gateways/{id}` | | 204, its token revoked; 409 while a database, directory or identity uses it (`admin`) |
| `GET /v1/secrets/gateways/{id}/connect` | | The gateway's WebSocket (rolld; its token, or an identity's login made for it) |
| `GET /cloud-accounts` | | `CloudAccount[]` (`read` on the project) |
| `POST /cloud-accounts` | `{name, cloud: "gcp", audience, service_account}` or `{name, cloud: "azure", tenant, client_id, scope?}` | 201 `CloudAccount` (`admin`; 400 when a made-up project's token is taken, or this project's isn't) |
| `DELETE /cloud-accounts/{id}` | | 204; 409 while a key links to it (`admin`) |
| `GET /kubernetes` | | `KubernetesCluster[]` (`read` on the project; never the token or the CA) |
| `POST /kubernetes` | `{name, api_url, ca_pem?, token}` | 201 `KubernetesCluster` (`admin`; calls `GET /api` with the token first: 400 with the host and status) |
| `DELETE /kubernetes/{id}` | | 204; 409 while a key links to it or a lease is live (`admin`) |
| `GET /github-apps` | | `GitHubApp[]` (`read` on the project; never the key) |
| `POST /github-apps` | `{name, app_id, installation_id, private_key, repositories, permissions, api_base?}` | 201 `GitHubApp` (`admin`; gets and revokes a token first: 400 with GitHub's reason) |
| `DELETE /github-apps/{id}` | | 204; 409 while a key links to it or a lease holds a token (`admin`) |
| `GET /v1/secrets/oidc/.well-known/openid-configuration` | | roll's issuer's discovery document (public) |
| `GET /v1/secrets/oidc/jwks` | | roll's issuer's public keys (public; served without clouds too, for the audit log's legacy digests): the key signing, the next one before it signs, the one it replaced until that one's tokens expire |
| `POST /v1/secrets/issuer/rotate` | `{now?, retire?}` | `{kid, signs_from, pulled}` (the operator's): the next issuer key, published now, signing two days later; the one already waiting, if any. `{now: true, retire: kid}` (both or neither): a new key signs now and `kid` leaves the JWKS at once (`pulled`), and the digest keys' `legacy` list; 404 for a kid not in it |
| `GET /v1/secrets/audit/jwks` | | `{keys, legacy, pulled}` (public): the audit log's digest keys (Ed25519 JWKs, retired ones too, never pulled ones), the issuer keys whose legacy digests count (RSA JWKs), and `[{kid, pulled_at}]` for keys pulled in an emergency, either kind. What `audit verify --file` takes |
| `POST /v1/secrets/audit/key/rotate` | `{now?, retire?}` | `{kid, pulled, signed}` (the operator's): a new digest key signs now and the previous one retires. `{now: true, retire: kid}` (both or neither): `kid` is pulled too, and every project's chain is signed under the new key before the answer (`signed`, how many); 404 for a kid not roll's or pulled already |
| `GET /cas` | | `Ca[]` (`read` on the project; certificates, never keys) |
| `POST /cas` | `{name, domains, allow_ip?, max_days?}` | 201 `Ca` (`admin`) |
| `DELETE /cas/{id}` | | 204; 409 while a key links to it (`admin`) |
| `GET /cas/{id}/certificates` | | `CaCertificate[]`: `{serial, names, source, not_after, issued_at, issued_by, revoked_at?}`, what it signed that hasn't expired, newest first (`read`) |
| `POST /cas/{id}/revoke` | `{serial}` | 204; 404 for a serial it didn't sign or that has expired (`write`, audited) |
| `GET /v1/secrets/crl/{id}` | | the CA's CRL, DER (`application/pkix-crl`) or `?format=pem`; no token |
| `POST /v1/secrets/ocsp/{id}` | an OCSP request (DER) | an OCSP response (`application/ocsp-response`); no token |
| `GET /v1/secrets/ocsp/{id}/{request}` | | the same, the request base64 and URL-encoded (RFC 6960 A.1); no token |
| `POST /cas/{id}/sign` | `{csr, days?, ttl_minutes?}` | `{certificate, ca, serial, names, not_after}` (`write`, audited); 400 for a name outside the policy or a lifetime past `max_days` |
| `GET /cas/{id}/acme` | | `{directory, pre_authorize, challenges, bindings: AcmeBinding[]}` (`read`; `challenges` is `http-01`, `dns-01` and `tls-alpn-01`, or none without rolld's checks); a binding is `{id, label, names, expires_at, created_at, created_by, revoked_at, revoked_by, used_at, accounts}` |
| `PATCH /cas/{id}/acme` | `{pre_authorize}` | the same (`admin`, audited) |
| `POST /cas/{id}/acme/eab` | `{label?, names?, days?}` | 201 `AcmeBinding` with `key_id`, `hmac_key` (base64url, this answer only) and `directory` (`admin`, audited) |
| `DELETE /cas/{id}/acme/eab/{eab}` | | 204: its accounts stop at once (`admin`, audited) |
| `GET /v1/secrets/acme/{ca}/directory` | | the directory (RFC 8555 §7.1.1), `externalAccountRequired: true`; no token ("ACME") |
| `HEAD /v1/secrets/acme/{ca}/new-nonce` | | 200 with `Replay-Nonce` (`GET`: 204); no token |
| `POST /v1/secrets/acme/{ca}/new-account` | JWS (`jwk`) of `{contact?, termsOfServiceAgreed?, onlyReturnExisting?, externalAccountBinding}` | 201 and `Location` (200 for the key's existing account); `externalAccountRequired` without a binding |
| `POST /v1/secrets/acme/{ca}/acct/{id}` | JWS (`kid`): empty, `{contact}` or `{status: "deactivated"}` | the account |
| `POST /v1/secrets/acme/{ca}/acct/{id}/orders` | JWS, empty | `{orders}`: its orders that haven't expired |
| `POST /v1/secrets/acme/{ca}/new-order` | JWS of `{identifiers}` (`dns`, `ip`) | 201 order (a name the account proved reuses its valid authorization); `rejectedIdentifier` outside the CA's policy or the binding's names; `rateLimited` past 300 a day |
| `POST /v1/secrets/acme/{ca}/order/{id}` | JWS, empty | the order: `pending`, `ready`, `processing`, `valid` (with `certificate`) or `invalid` |
| `POST /v1/secrets/acme/{ca}/order/{id}/finalize` | JWS of `{csr}` | the order, `valid`; `orderNotReady`, `badCSR` (names other than the order's), `unauthorized` (a name the CA's policy or the binding no longer takes) |
| `POST /v1/secrets/acme/{ca}/authz/{id}` | JWS: empty or `{status: "deactivated"}` | the authorization and its challenges |
| `POST /v1/secrets/acme/{ca}/chall/{id}` | JWS: empty, or `{}` to validate now | the challenge, `valid` or `invalid` with its `error` |
| `POST /v1/secrets/acme/{ca}/cert/{serial}` | JWS, empty | `application/pem-certificate-chain`: the certificate, then the CA's |
| `POST /v1/secrets/acme/{ca}/revoke-cert` | JWS (`kid`, or the certificate's key as `jwk`) of `{certificate, reason?}` | 200; on the CRL at once; `alreadyRevoked` |
| `POST /v1/secrets/acme/{ca}/key-change` | JWS wrapping the new key's JWS of `{account, oldKey}` | the account; 409 and `Location` if another account has the new key |
| `GET /ssh-cas` | | `SshCa[]` (`read` on the project; public keys, never private ones) |
| `POST /ssh-cas` | `{name, principals, max_hours?, host_domains?, max_host_days?}` | 201 `SshCa` (`admin`) |
| `DELETE /ssh-cas/{id}` | | 204; 409 while a key links to it (`admin`) |
| `POST /ssh-cas/{id}/sign-user` | `{public_key, principals, ttl_minutes?}` | `{certificate, serial, principals, valid_before}` (`reveal`, audited) |
| `POST /ssh-cas/{id}/sign-host` | `{public_key, hosts, days?}` | `{certificate, known_hosts, serial, hosts, valid_before}` (`admin`, audited); 400 for a name outside `host_domains` or a weak key |
| `GET /transit` | | `TransitKey[]` (`read`; versions and dates, never key material) |
| `POST /transit` | `{name, key?, vault_version?}` | 201 `TransitKey` (`admin`; `key`: one brought, 32 bytes base64) |
| `PATCH /transit/{name}` | `{min_decrypt_version}` | `TransitKey` (`admin`) |
| `DELETE /transit/{name}?confirm=` | | 204; 400 unless `confirm` repeats the name (`admin`) |
| `POST /transit/{name}/rotate` | `{key, vault_version?}` or nothing | `TransitKey` (`admin`); 409 for a Vault version it has |
| `POST /transit/{name}/encrypt` | `{plaintext, context?}` or `{items: […]}` | `{ciphertext}` or `{items: [{ciphertext}]}` (`write`) |
| `POST /transit/{name}/rewrap` | `{ciphertext, context?}` or `{items: […]}` | `{ciphertext}` or `{items}` (`write`) |
| `POST /transit/{name}/decrypt` | `{ciphertext, context?}` or `{items: […]}` | `{plaintext}` or `{items}` (`reveal`, audited) |
| `POST /transit/{name}/datakey` | `{context?}` | `{plaintext, ciphertext}` (`reveal`, audited) |
| `POST /transit/{name}/sign` | `{input}` or `{items: […]}` | `{signature}` or `{items}` (`write`; Ed25519) |
| `POST /transit/{name}/verify` | `{input, signature}` or `{items: […]}` | `{valid}` or `{items}` (`read`) |
| `POST /transit/{name}/hmac` | `{input}` or `{items: […]}` | `{hmac}` or `{items}` (`write`; HMAC-SHA256) |
| `POST /transit/{name}/hmac/verify` | `{input, hmac}` or `{items: […]}` | `{valid}` or `{items}` (`write`: it answers guesses) |
| `GET /transit/{name}/public` | | `TransitPublicKey[]`: `{version, public_key (base64), pem, jwk}` for every version (`read`) |
| `GET /v1/secrets/whoami` | | `{principal, project?, scopes?}` |
| `GET /v1/secrets/search?key=` | | `Place[]`: every project, environment and folder holding `key` (`PREFIX*` for a prefix) that the caller may read; never values |

- 400: a bad slug, path, key, value, body or query.
- 401: no credential, or a token that doesn't exist, is revoked or expired.
- 403: the caller may not do this (`{"code": "forbidden"}`).
- 404: no such project, environment, folder, secret, version, change, request, lease or token.
- 410: a secret request answered as often as it allowed, expired or cancelled (`gone`).
- 409: a slug that is taken (`conflict`), a write to a protected environment
  (`approval_required`), a request that is `stale`, the last standard environment.
- 503: no deployment key on a node that shares the system database (`no_key`).

### Shapes

```ts
type Environment = {
  id: string; slug: string; name: string; position: number; kind: "standard" | "ephemeral";
  base: string | null; labels: Record<string, string>; expires_at: number | null;
  protected: boolean; approvals: number; approvers: string[]; created_at: number; secrets: number;
  max_age_days: number | null; notify: string | null; trusted: boolean; // trusted: an ephemeral one that may read a protected base
  branches?: Branch[]; // an ephemeral one's branched resources (absent: none)
};
type Branch = { kind: string; parent: string }; // a resource a preview branched: links to it aren't followed
type Followed = { name: string; kind: string; parent: string; field: string }; // a key left out for roll previews to set from its branch
type Ensure = Omit<NewEnvironment, "slug"> & { replace?: boolean }; // PUT /environments/{env}; branches: replaces the list, absent keeps it
type NewEnvironment = {
  slug: string; name?: string; kind?: "standard" | "ephemeral"; base?: string; labels?: Record<string, string>;
  expires_at?: number; ttl_minutes?: number; secrets?: Op[]; // ephemeral: created in one call with its overrides
  trusted?: boolean; // ephemeral: may read a protected base (admin on it)
  branches?: Branch[]; // ephemeral: the resources its preview branched (roll previews or the operator)
};
type Folder = { path: string; name: string; secrets: number; folders: number };
type Import = { project?: string; environment: string; path: string }; // project: another project's shared folder
type Export = { project: string; created_at: number; created_by: string };
type Link = { kind: string; ref: string; field: string; mode: "live" | "managed" | "dynamic"; options?: Record<string, unknown> };
type Secret = {
  id: string; key: string; path: string; type: "shared" | "personal"; version: number;
  comment: string; tags: string[]; link: Link | null; fp: string; references: string[];
  updated_at: number; updated_by: string;
  source: { kind: "own" } | { kind: "import"; environment: string; path: string } | { kind: "base"; environment: string };
  overridden: boolean; // a personal value of the caller replaces it
  decoy?: Decoy; // only for the project's admins ("Decoys"); absent for everyone else, as on any key
};
type Decoy = { kind: "rlt" | "postgres"; created_at: number; created_by: string; uses: number; used_at: number | null; used_ip: string | null };
type NewDecoy = { key: string; kind: "rlt"; path?: string } | { key: string; kind: "postgres"; resource: string; path?: string };
type SetSecret = { value?: string; comment?: string; tags?: string[]; type?: "shared" | "personal"; link?: Link | null; message?: string; expires_at?: number | null; if_version?: number };
type Op =
  | { op: "set"; key: string; value?: string; comment?: string; tags?: string[]; type?: "shared" | "personal"; link?: Link | null; if_version?: number }
  | { op: "delete"; key: string; type?: "shared" | "personal"; if_version?: number };
type Batch = { path?: string; message?: string; ops: Op[]; if_seq?: number };
type Written = { change: Change | null; secret: Secret | null; warnings: string[] };
type Applied = { change: Change | null; items: Item[]; warnings: string[]; request?: Request }; // change: null when nothing changed; request: opened instead (promote into a protected environment)
type Item = { key: string; path: string; type: "shared" | "personal"; what: string[]; from_fp: string | null; to_fp: string | null };
type Change = { seq: number; environment: string; path: string; message: string; by: string; at: number; keys: number; kind: "write" | "rollback" | "promote" | "rotate" | "request" | "rename" };
type ChangeDetail = Change & { items: Item[]; state?: { key: string; path: string; version: number; fp: string; comment: string; tags: string[]; link: Link | null; at: number }[] };
// what: added | changed | deleted | comment | tags | link | rotated
type Version = { version: number; seq: number; op: "set" | "delete"; what: string[]; purged: boolean; fp: string | null; comment: string; tags: string[]; link: Link | null; at: number; by: string };
type Values = { values: Record<string, string>; problems: { key: string; problem: string }[]; leases: string[]; dynamic: string[]; linked: string[]; plain?: string[]; withheld: string[]; sensitive: string[]; followed: Followed[] }; // plain: of linked, keys whose link field isn't a credential (a host, a port), which a consumer hiding secret values in logs may show (every other value is a secret); followed: keys linked to what the ephemeral environment's preview branched, not in values; leases: ids of dynamic credentials issued; dynamic: the keys holding one; linked: keys from any link (addresses a consumer on another network rewrites)
type Values = { values: Record<string, string>; problems: { key: string; problem: string }[]; leases: string[]; dynamic: string[]; linked: string[]; plain?: string[]; followed: Followed[]; digest?: string }; // plain: as above; digest (with ?digest=1, not with seq or at): the folder's `…/digest`, taken just before this read; leases: ids of dynamic credentials issued; dynamic: the keys holding one; linked: keys from any link (addresses a consumer on another network rewrites)
type Grid = { path: string; environments: string[]; rows: { key: string; cells: Record<string, Cell | null>; status: "same" | "different" | "missing"; required: boolean }[]; folders: string[]; previews: number };
type Reach = { environment: string; path: string; key: string | null; protected: boolean; untrusted: boolean; tokens: Holder[]; identities: Holder[]; syncs: { id: string; kind: string; target: unknown; last_synced_at: number | null }[]; projects: string[]; imported_by: string[]; truncated: boolean; readers: { actor: string; last_at: number; reads: number }[]; uncounted: string }; // untrusted: checks, sandboxes and previews may read it (not protected)
type Holder = { id: string; name: string; actions: Action[]; expires_at: number | null; last_used_at: number | null; created_by: string };
type Insights = { path: string; max_age_days: number | null; reads_since: number; keys: { key: string; version: number; updated_at: number; updated_by: string; last_read_at: number | null; last_read_by: string | null; reads_30d: number; stale: boolean }[] };
type Required = { key: string; path: string; comment: string; type?: "url" | "int" | "port" | "bool" | "json" | "email"; pattern?: string; owner?: string /* who supplies it */; password?: { length: number; classes?: ("lower" | "upper" | "digit" | "symbol")[]; symbols?: string } };
type Sink = { id: string; url: string; status: "new" | "ok" | "error"; error: string | null; cursor: number; delivered_at: number | null; created_at: number; created_by: string };
type ProjectFindings = { project: string; project_name: string; findings: Finding[] };
type Finding = { issue: "decoy_used" | "expired" | "rotation_failing" | "revoke_failing" | "missing" | "exposed" | "public" | "weak" | "operator_made" | "expiring" | "certificate_expiring" | "stale" | "unread" | "unreferenced"; environment: string; path: string; key: string; detail: string; owner?: string };
type Rotation = { environment: string; path: string; keys: string[]; kind: string; ref: string; name: string; rotate_days: number | null; rotate_window: string | null; grace_minutes: number; issued_at: number | null; next_due: number | null; last_error: string | null; failed_at: number | null };
type Rotator = { id: string; name: string; host: string; created_at: number; created_by: string };
type SaasRotator = { id: string; name: string; vendor: "sendgrid" | "twilio" | "okta" | "entra" | "auth0"; target: Record<string, unknown>; created_at: number; created_by: string };
type KubernetesCluster = { id: string; name: string; api_url: string; has_ca: boolean; created_at: number; created_by: string };
type Share = { id: string; note: string; views_left: number; created_by: string; created_at: number; expires_at: number; opened_at: number | null; open: boolean };
type NewValueRequest = { keys: string[]; note?: string; ttl_minutes?: number /* 5–43200, default 10080 */; answers?: number /* 1–10, default 1 */ };
type ValueRequest = { id: string; environment: string; path: string; keys: string[]; note: string; answers_left: number; created_by: string; created_at: number; expires_at: number; answered_at: number | null; cancelled_at: number | null; open: boolean; outcome: { change: number } | { request: string } | null; link?: string };
type Sealed = { v: 1; epk: string; nonce: string; ct: string }; // roll-seal-v1 to the request's public_key, associated data `roll-secret-request:<id>:<KEY>`
type Cell = { fp: string; version: number; updated_at: number; link: Link | null; reference: boolean; source: "own" | "import" | "base"; comment: boolean; personal: boolean; misfit?: string };
type Compare = { from: string; to: string; path: string; rows: { key: string; status: "same" | "different" | "only_from" | "only_to" | "hidden"; from: number | null; to: number | null }[] };
type Request = { id: string; environment: string; path: string; message: string; by: string; at: number; state: "open" | "approved" | "rejected" | "applied" | "closed" | "stale"; items: Item[]; reviews: { by: string; verdict: "approve" | "reject"; comment: string; at: number }[]; approvals: number; approvals_needed: number; applied_seq: number | null };
type Compare = { from: string; to: string; path: string; rows: { key: string; status: "same" | "different" | "only_from" | "only_to"; from: number | null; to: number | null }[] };
type Request = { id: string; environment: string; path: string; message: string; by: string; at: number; state: "open" | "approved" | "rejected" | "applied" | "closed" | "stale"; items: Item[]; reviews: { by: string; verdict: "approve" | "reject"; comment: string; at: number; gates?: (string | null)[] }[]; approvals: number; approvals_needed: number; gates: { path?: string; approvals: number; approvers?: string[]; approved: number }[]; applied_seq: number | null };
type FolderPolicy = { path: string; approvals: number; approvers: string[]; created_at: number; created_by: string };
type LinkTarget = { kind: string; ref: string; name: string; fields: { field: string; label: string; modes: Link["mode"][]; secret: boolean; suggest: string; credential?: string }[] };
type Lease = { id: string; environment: string; path: string; key: string; kind: "dynamic" | "retired"; link: Link | null; consumer: string; issued_at: number; expires_at: number; revoked_at: number | null };
type Identity = { id: string; name: string; kind: "oidc" | "aws"; arn?: string; server_id?: string; issuer: string; audience: string; subject: string; claims: Record<string, string>; environments: string[] | null; paths: string[] | null; actions: Action[]; ttl_minutes: number; created_at: number; created_by: string; last_used_at: number | null; last_subject: string | null; allowed_ips: string[] | null; pinned_keys?: number; gateway?: string }; // gateway: it logs that gateway in (no environments, paths or actions)
type Gateway = { id: string; name: string; token_id?: string; created_at: number; created_by: string; status: { online: boolean; node?: string; version?: string; allow: string[]; connected_at?: number; last_seen_at?: number } };
type NewIdentity = { name: string; environments?: string[]; paths?: string[]; actions: Action[]; ttl_minutes?: number; allowed_ips?: string[]; gateway?: string } & ({ kind?: "oidc"; issuer: string; audience: string; subject: string; claims?: Record<string, string>; jwks?: string } | { kind: "aws"; arn: string }); // claims: a name or a JSON pointer ("/realm_access/roles") → a value or a * pattern (workload identities, "Claims")
type Token = { id: string; name: string; environments: string[] | null; paths: string[] | null; actions: Action[]; expires_at: number | null; created_at: number; created_by: string; last_used_at: number | null; prefix: string; allowed_ips: string[] | null };
type NewToken = { name: string; environments?: string[]; paths?: string[]; actions: Action[]; expires_at?: number; ttl_days?: number; allowed_ips?: string[] };
type Action = "read" | "reveal" | "write" | "delete" | "admin";
type Sync = {
  id: string; environment: string; path: string; kind: string; target: Record<string, unknown>;
  keys: string[] | null; prefix: string; status: "pending" | "ok" | "error"; error: string | null;
  last_digest: string | null; last_synced_at: number | null; synced: number; // names at the target
  signed: boolean; created_at: number; created_by: string;
};
type NewSync = { environment: string; path?: string; kind: string; target: Record<string, unknown>; keys?: string[]; prefix?: string };
type SyncKind = { kind: string; label: string; values: boolean; signed: boolean; fields: { name: string; label: string; required: boolean; placeholder: string }[] };
type AuditEntry = { id: number; seq?: number; hash?: string; at: number; actor: string; action: string; environment: string | null; path: string | null; keys: string[]; summary: string; ip: string | null; approval?: Approval; digest?: string };
type AuditVerification = { ok: boolean; from: number | null; through: number | null; signed_at: number | null; head: number | null; rows: number; digests: number; unchained: number; broken: { seq?: number; reason: string; detail: string } | null; legacy?: { signer: "issuer key"; digests: number; through: number }; pulled?: { kid: string; pulled_at: number; digests: number; first: number; last: number; countersigned: number | null }[] }; // digests: the ones that vouch (digest key and legacy); pulled: digests on pulled keys, countersigned by the next good digest's own row, or null (a `pulled` break)
// The approval that let the request through roll-access's gate (docs/permissions.md, "What a
// product is told about an approval"): an `ask` approved (no `until`), or a rule an approval
// remembered for a while (`until`: when it lapses). CSV exports carry `approval` and `approved_by`
// (the approvers joined with ", ").
type Approval = { id: string; by?: string[]; until?: number }; // by: who approved, in order
```

## Tables

```sql
projects(project_id PK, dek TEXT, seq INTEGER, created_at)          -- dek: the sealed data key
environments(id PK, project_id, slug, name, position, kind, base_id, labels, expires_at,
             protected, approvals, approvers, created_at, UNIQUE(project_id, slug))
folders(env_id, path, created_at, PRIMARY KEY (env_id, path))
imports(env_id, path, position, from_env_id, from_path, PRIMARY KEY (env_id, path, position))
secrets(id PK, project_id, env_id, path, key, owner, version, value BLOB, fp, comment, tags, link,
        link_state, refs, updated_at, updated_by, UNIQUE(env_id, path, key, owner))  -- refs: the references' names, so listings decrypt nothing
versions(secret_id, version, seq, env_id, path, key, owner, op, value BLOB, fp, comment, tags, link,
         at, by, PRIMARY KEY (secret_id, version))
changes(project_id, seq, env_id, path, kind, message, by, at, keys, PRIMARY KEY (project_id, seq))
requests(id PK, project_id, env_id, path, message, by, at, state, ops BLOB, items, reviews, applied_seq)
leases(id PK, project_id, env_id, key, path, kind, link, handle, consumer, issued_at, expires_at,
       revoked_at)
tokens(id PK, project_id, name, hash UNIQUE, prefix, environments, paths, actions, expires_at,
       created_at, created_by, last_used_at, revoked_at, identity)  -- identity: a login's
identities(id PK, project_id, name, issuer, audience, subject, claims, environments, paths,
       actions, ttl_minutes, created_at, created_by, last_used_at, last_subject)
audit(id INTEGER PK, project_id, at, actor, action, env_id, path, keys, summary, ip, approval,
      seq, hash, digest)  -- UNIQUE (project_id, seq); digest: an audit.digest row's JWS
audit_heads(project_id PK, seq, hash, at, digest_seq, digest_at, start_seq, mac)  -- mac: HMAC under ROLL_SECRET_KEY
audit_keys(kid PK, sealed_dek, private BLOB, x, legacy, created_at, retired_at, pulled_at)
      -- the digest key (Ed25519): private sealed with kid + legacy as AAD, wiped when pulled;
      -- legacy: issuer kids whose RS256 digests from before it count
syncs(id PK, project_id, env_id, path, kind, target, keys, prefix, secret BLOB, status, error,
      last_digest, state, last_synced_at, running_until, retry_at, checked_at, created_at, created_by)
      -- secret: the sealed signing secret; state: {name: stamp} of what was pushed (no values)
value_requests(id PK, project_id, env_id, path, keys, note, public_key, seal_key BLOB, answers_left,
      created_by, created_at, expires_at, answered_at, cancelled_at, outcome)
      -- seal_key: the request's X25519 secret key, sealed with the data key; NULL once it closes
```

`owner` is `''` for a shared secret and the principal for a personal one. A deleted secret keeps its
row with `op = 'delete'` in `versions` and is removed from `secrets`, so `(env, path, key, owner)` is
free again and its history stays readable by key.

## Metrics

rolld's access metrics count roll secrets as its own product (`product: secrets`: `Latency`,
`Requests`, `Http5xx`), and a line a minute (JSON logs, CloudWatch's Embedded Metric Format, beside
the cluster's) carries what roll secrets did since the last one and how it stands, by `service`
and `product`. Numbers only: never a value, a key's name or a project.

| Metric | What |
| --- | --- |
| `SecretsReads` | Reads of values: a folder read or resolved, or a key revealed (people, tokens, runtimes) |
| `SecretsDynamicIssued` | Dynamic credentials issued (a lease each) |
| `SecretsLeasesRevoked`, `SecretsRevokeFailures` | Leases ended, and revocations a product refused (retried) |
| `SecretsSyncsPushed`, `SecretsSyncFailures` | Sync pushes that worked and that failed |
| `SecretsRotations`, `SecretsRotationFailures` | Scheduled rotations that worked and that failed |
| `SecretsKmsRefusals` | A customer's KMS refusing to open a project's data key |
| `SecretsLeasesLive`, `SecretsSyncsFailing`, `SecretsRotationsFailing` | Gauges: leases not ended, syncs whose last push failed, credentials whose last rotation failed |

Each node counts what it did, so sum the counters across nodes. The gauges read the store every
node shares, so every node reports the same number: read them with `Maximum`, never `Sum`.

Alarms worth setting: `SecretsKmsRefusals` above zero (a customer's key policy changed),
`SecretsRevokeFailures` and `SecretsRotationsFailing` staying above zero (health names them),
`SecretsSyncsFailing` rising.

## Performance

`cargo run -p roll-secrets --release --example bench -- 9000` times the hot paths on one
environment of 9,000 keys plus 900 references, on SQLite or, with `ROLL_CODE_TEST_DATABASE`, on
Postgres. Measured 2026-11-18 on ai-server, pinned to 12 CPUs (`taskset -c 0-5,18-23`): the
medians of three runs each, before (the code as of `42a3bbe1b`) and after the polling path's cache,
alternated, under load averages of 27 to 73. A poll is 100 calls; its p95 is in brackets.

| | SQLite before | SQLite after | Postgres before | Postgres after |
| --- | --- | --- | --- | --- |
| a 304 (`If-None-Match`, unchanged) | 99 ms (144) | 0.1 ms (0.1) | 169 ms (241) | 3.0 ms (13) |
| `…/digest`, unchanged | 108 ms (138) | 0.1 ms (0.1) | 157 ms (213) | 2.5 ms (7) |
| the first digest after a change | 108 ms | 71 ms (88) | 157 ms | 98 ms* |
| one change of 9,900 keys | 483 ms | 506 ms | 12.1 s | 11.7 s |
| one key's change | 13 ms | 8 ms | 12 ms | 10 ms |
| grid (every standard environment) | 88 ms | 91 ms | 117 ms | 111 ms* |
| list | 51 ms | 53 ms | 76 ms | 61 ms* |
| values, resolved | 166 ms | 170 ms | 232 ms | 210 ms* |
| insights | 54 ms | 56 ms | 66 ms | 65 ms* |
| compare | 73 ms | 79 ms | 104 ms | 96 ms* |
| health (the whole project) | 64 ms | 59 ms | 82 ms | 78 ms* |

\* Postgres's last two rounds after the change ran under loads of 70 and above, and the paths the
change doesn't touch then took up to 4× as long in both builds. Those rows give the run without the
digest cache but with its narrower read (the same code for them), alternated in the same session.
The test database had 4,650 leftover test schemas, which slows every run a little more than the last.

At 1,000 pods polling every 30 s (~33 calls a second) a folder that didn't change cost about 5.6
cores on Postgres (3.3 on SQLite) to answer 304s; now it costs about 0.1. A digest is computed
once after each change and then reused (`Secrets::digest_of`, read.rs):
- **Narrower and once.** A digest reads only the columns that go into its hash, and reads each
  folder once: a reference into the folder being digested finds its target in rows already read
  (when the caller may read every import and base exactly as it may reveal them). Those two changes
  alone halve the first digest (and were the 304's cost before the cache: 62 ms on SQLite, 71 ms on
  Postgres).
- **Reused while the database says nothing changed.** Each node keeps a folder's digest by
  project, environment, folder, principal and whether the caller is untrusted. It reuses the digest
  only while one read of the database answers the same: the project's change number (every write
  of a stored value takes one in its own transaction), its environments, imports and required
  keys, and the change number and share of any folder another project shares into it. A write on
  any node moves it at once, and no node's memory decides whether a folder changed. A node keeps
  at most 10,000 digests, and at most 500 for one principal: past either, the least recently used
  goes (the principal's own first), so one reader cycling through many folders only ever pushes
  out its own, never another poller's.
- **Only for the same answers.** It records every permission the digest asked of the caller (an
  import's environment, a reference's folder) with its answer, and is reused only when the caller
  gets the same answers now. Each live link is checked as before: its generation (`source_changed`)
  is read again, and its value's fingerprint is read the same way, at most 10 s old. A digest
  where anything failed to read is never kept.

`tests/nodes.rs` (`a_digest_one_node_keeps_moves_at_once_with_what_another_node_changes`) keeps
a digest on one node and changes things on another: a write, an import, a write to the imported
folder, roll rotating a live link's credential (though the first node read the product's answer a
moment before). It also changes what the same caller may read. Each one moves the digest at once.
`links_read_live_values_issue_managed_credentials_rotate_and_lease_dynamic_ones` (`tests/api.rs`)
still covers 304s, and a product changing its own password.

Every lookup by key inside a request is a hash map (a grid of ~11k keys over 5 environments took
3.07 s before that, 0.43 s after, on a debug build).

## Next

Done and in PR #20:
- the store and its crypto (the deployment key from the environment, a file, or AWS KMS);
- rolld's mount and link providers (Postgres, Redis, streams, buckets, NoSQL, search: checked
  live against a real roll search cluster), and
  rotators for values outside roll;
- `roll secrets`, `roll secrets setup` and `roll run [--watch]`;
- checks, sandboxes and previews, resolving untrusted; preview environments untrusted themselves;
- leaked secrets don't land, plus scans of a repo's default branch and history, and `roll secrets
  scan [--staged]` before a commit;
- credentials roll doesn't hold yet, by their shape: a notice on a change (a blocker with
  `roll.toml`'s `[secrets] unstored_credentials = "block"`), `.roll/secrets-allow`, and `roll secrets
  scan` listing them, refusing them `--staged`, and `--store` moving one in; the change page's
  "Store in roll" storing one from the change itself into the project the repo is trusted for;
- required and typed keys; insights, a daily notice of each environment's health, and expiry dates for values that expire
  outside roll; weak credentials; values as they were after any change; a protected environment's values that are also set in an unprotected one; read
  alerts (a machine's new address, many reveals in an hour);
- one-time sharing, and sharing across projects;
- syncs: GitHub, GitLab CI, CircleCI, Bitbucket Pipelines, Terraform Cloud, Vercel, Netlify, Render, Fly.io, Railway,
  Heroku, DigitalOcean App Platform, Cloudflare Workers, AWS
  Parameter Store and Secrets Manager, Google Secret Manager, Azure Key Vault, Kubernetes, sealed webhooks and deploy notifications;
- audit sinks, purge and retention, workload identities (OIDC logins for CI), tokens and
  identities pinned to client addresses; encrypted project backup and restore;
- the dashboard's Secrets surface, with the deployment key's source and a rotation's progress,
  suggested links for the canvas's resources, and one key set in several environments from the
  overview (one change each, a request where protected), and each environment's activity (its
  changes, reads, requests and syncs' pushes as one timeline, by key);
- finding a key across every project the caller may read (`roll secrets find`, the dashboard's
  "Find a key"), for rotating an organisation-wide key;
- exports as a Docker `--env-file` and a Kubernetes Secret manifest, next to dotenv, shell, JSON and
  YAML; `roll secrets render [--watch --exec]` fills `${KEY}` in a config file and keeps it current;
  `roll secrets check` compares an environment with a `.env.example`;
- the fixes from three security reviews and a phone-width pass;
- every route checked to be in the HTTP table (`every_route_is_in_the_http_contract`), which
  roll-access's catalogue is checked against.

Then change policies per folder (#53), as Infisical's approval policies on secret paths.

A ninth independent review (2026-11-14, of `roll run`'s masking and the eighth review's fixes)
found no high-severity issue; it found a held tail flushed on a timer (a block-buffered program
pausing mid-value leaked it in halves), masking on by default for data written to files and pipes,
background processes holding roll open, `--watch` dropping SIGTERM, a SIGINT judged by stdin (#54),
a folder's admin counting toward a protected environment's own gate and an import guard outside its
transaction (#53), a Vault refusal told by an error's text (#52), and a write during a watch's
re-read going unnoticed (#49): all fixed and checked live, tested where a test guards behaviour.
Since then a values answer carries the folder's digest, taken just before its read, and `--watch`
compares from those answers, which closes the last window (#49).
Every stacked pull request's API tests (#49, #50, #51, #53, #55, #57) also pass on Postgres, and
#49 adds **Copy reference** to a key's menu (`roll://ENV[/folder]/KEY`, as 1Password's). Every
stacked pull request is mergeable, and #20 is level with `main`.
Then roll secrets for VS Code (#58): hovers and warnings where code reads a variable, names
completed, and `.env` values moved into roll as `roll://` references (checked in code-server).
Then `roll completions` and direnv documented (#59).
Docker builds (BuildKit secrets) and Compose are documented and checked. Then a server's
credential under a name frameworks put in the browser bundle (`NEXT_PUBLIC_`, `VITE_`…) is
flagged on write and in health (#60). Measured again (9,900 keys, alternating with #20): health
is unchanged by #60 and stays under the grid; a values answer takes its digest only when a watch
asks (`digest=1`, #49), since that costs about a read.
Then reach (#61): for a folder or key, the tokens and identities that reach it, whether untrusted
code can, where it flows (syncs, shares, imports), and who read it in 30 days.

A tenth independent review (2026-11-14, of ESO, renewing logins, pinned keys and the ninth review's
fixes) found no high-severity issue; it found ESO writing an unresolved key as an empty value and
taking a query through its key (#55: `strict=1`, `urlquery`, checked live on k3s), stdout unmasked in
Jenkins, Buildkite, pods and units and a JSON value's secret fields (#54), a login with no timeout
that dropped a working token and ignored 401s (#56), and no way to rotate pinned keys in place
(#57): all fixed. roll MCP has `secrets_reach` and `secrets_folder_policies` tools for #61 and #53.

An eleventh independent review (2026-11-14, of VS Code, completions, browser-bundle names, reach and
the tenth review's fixes) found two high-severity issues in #58, both fixed: "Move to roll" misread
multi-line and commented `.env` entries (it now reads them as the CLI does and replaces the whole
entry), and a repository's settings could send the token elsewhere (`roll.apiUrl` and
`roll.project` come from your own settings only). It also found masking that changed data (#54:
stdout is masked only in CI and never into a file; `ROLL_MASK=1` for pods and units), re-keyed
identities outliving their re-keyer (#57), readers missed through imports and previews (#61), a
kind of credential shown to read-only callers and references not judged (#60), a stalled renewal
(#56) and a refused strict read keeping its leases (#55): all fixed.

A twelfth independent review (2026-11-14, of the eleventh review's fixes) found no high-severity
issue; it found a login racing a key replacement could still get a token (#57: the login's write
checks the keys it verified), Jenkins' file log unmasked (#54: documented, `ROLL_MASK=1`, since a
file can't be told from a dump), late logins and late 401s overwriting fresh tokens (#56), reach
looking one step away and at unshared importers (#61), a write's warning naming a credential's
kind without reveal (#60), and `.env` corner cases and a repository choosing the environment (#58):
all fixed.

A thirteenth review (2026-11-14, narrow: the twelfth review's fixes) found reach naming another
project's internal environments and walking past where reads go (#61: one hop into a shared
project, named only as a project, capped at 256 folders), reference warnings shown without reveal
(#20), `.env` files over roll's limits (#58) and a background login outliving a 401 (#56): all
fixed. The pinned-keys, relogin and masking fixes it checked were correct.
Reach measured at its cap (300 importing folders): 5–7 ms on SQLite and 120–150 ms on Postgres, once
the audit is read per project rather than per folder (#61).

Then (2026-11-14, latest): `roll run` masks values in what the command prints when it isn't a
terminal (#54), as 1Password's `op run` and GitHub Actions do; pulling into Kubernetes with External
Secrets Operator (#55: its webhook provider, checked against ESO 2.15 on k3s) and
`values?dynamic=false` for readers that can't renew a lease. #50, #51 and #53 merge cleanly
together, and the merged tree passes roll-secrets', rolld's auth and the dashboard's tests. #52's
import dialog names Vault trees and AWS (screenshots on #52). `roll secrets kubernetes` and the
Export menu print ESO's manifests with a fresh token (#55), and a workload identity's login renews
itself (#56), so a pod running `roll run --watch` needs no stored token, as with Vault's Kubernetes
auth; and identities pin their issuer's keys (#57), so clusters roll can't reach (k3s, kind,
private EKS) log pods in too (checked on k3s, with #56: a 5-minute login renewed by itself).
`roll run`'s masking was measured and made 5x faster (#54: 330 MB/s with 300 values and a 3 KB
key).

An eighth independent review (2026-11-14, of moving in, change policies per folder and the
seventh review's fixes) found no high-severity issue; it found folder gates a reviewer, a
promotion, an import or a folder delete could get around, apply checking the gates outside its
transaction, backups without folder policies (#53), Vault walks that took any error for an empty
folder and had no bound, colliding AWS keys (#52), and a `--watch` that restarted on every poll
after its references changed (#49): all fixed, the Vault and AWS ones checked live. Screenshots
of folder policies and request gates are on #53.

A seventh independent review (2026-11-14, of RabbitMQ, the OpenSearch names and the sixth
review's fixes) found no high-severity issue; it found a RabbitMQ template copied in part (an
exchange left open), `render --watch` rendering twice, throttled OCSP answers marked cacheable,
repeated flavor lookups, and the short public-suffix list: fixed, or documented for the last.
Moving in from Vault trees and AWS Secrets Manager is #52 (checked against Vault 1.20 and
LocalStack).

Then RabbitMQ outside roll (#39, merged up into #41 and #42; checked live against RabbitMQ 4) and
OpenSearch clusters' fields named as their clients read them (#51).

A sixth independent review (2026-11-14, of `roll://` references, check-and-set, OCSP, OpenSearch
links and the grant fix) found no high-severity issue; it found an OCSP route an anonymous caller
could make sign and cache per request, references whose leases were renewed at the wrong project,
check-and-set dropped on protected environments, and smaller ones (a provider asked before the
project check, a project name two owners share, render --watch, env-file errors, restore names,
public suffixes): all fixed, OCSP re-checked live. The project's tokens now read rules naming it by
id and show key-scoped ones (#30).

Later: the grid sends check-and-set too (#50), OCSP for project CAs (#44: checked with `openssl
ocsp`), search links on OpenSearch clusters (#51, agreed with the search thread), and a fix from roll
MCP's review on #30 (a grant naming the project by id reaches it inside roll secrets too).

Then (2026-11-14, latest): `roll://` references in files (#49: `roll run --env-file`, `render`) and
check-and-set writes (#50: `if_version`, the dashboard's editor), each its own pull request on #20.

A fifth independent review (2026-11-14, of host and user SSH certificates, ClickHouse,
certificates ending, `--rotate-due` and the fourth review's fixes) found no high-severity issue; it
found a CLI that would upload a private key given its path, SSH CAs that could claim a top-level
domain, best-effort signing audits, a stale-CRL race, ClickHouse's kill-then-drop order, a
known_hosts order and `--rotate-due --json` that didn't rotate: all fixed. Screenshots of the new
surfaces are on #44, #45 and #48.

Later still: SSH user certificates for keys people keep (#45: `…/sign-user`, `roll ssh --key`,
checked with an agent-held key against a real `sshd`) and `roll secrets rotations --rotate-due`
(#48).

Then (2026-11-14, latest): certificates ending as health findings (#44: `certificate_expiring`)
and ClickHouse outside roll (#39, merged up into #41 and #42: its HTTPS interface, `sha256_hash`
passwords, checked live against ClickHouse 25.8).

A fourth independent review (2026-11-14, of the rotations overview, transit signing, HMAC and
brought keys, CSR signing, revocation and the CRL, SQL Server) found no high-severity issue; it
found an ended SQL Server lease whose login could stay usable, generated passwords SQL Server's
policy refuses, deleted CAs undoing their revocations, a public CRL signed per request, P-384 CSRs
mislabelled, unrevocable serials, a batch failed by one retired version and a restore that forgot
brought keys: all fixed, tested where a test guards behaviour, and checked live (SQL Server 2022,
OpenSSL). Then SSH host certificates (#45, checked against a real `sshd`).

Built on 2026-11-14 (later), again without a merge: revoking certificates (#44: a list of what a
CA signed, `…/revoke`, revocation when a lease ends, a public CRL every certificate names, checked
with `openssl verify -crl_check`); SQL Server outside roll (#39, merged up into #41 and #42;
checked live against SQL Server 2022); bringing an encryption key (#47: Vault's `vault:v<n>:`
ciphertexts decrypt and rewrap, checked against Vault 1.20).

Built on 2026-11-14 from the last ranking, without waiting on a merge: a rotations overview (#48:
`GET …/rotations`, Health's "Managed credentials", `roll secrets rotations`); signing and HMAC
with encryption keys (#47: Ed25519 a stock library checks with the exported public key, HMAC
blind indexes, `transit(name)` in `@roll/secrets`); CSR signing (#44: `…/cas/{id}/sign`, checked
with OpenSSL's P-256 and RSA CSRs under `openssl verify -x509_strict`).

A third independent review (of #46 and #47) found four ways a sensitive value still reached a person
(older versions, change requests that add the tag, syncs and sandboxes counted as runtimes) and
guessing oracles, plus backups leaving out encryption keys: all fixed and tested. roll apps (#26)
now seals sensitive variables and redeploys when an environment's digest moves (#31 implements
it). An independent security review of #43–#45 found no cross-tenant path; it found managed
certificates that could lapse, an issuer key that a deployment-key rotation would have broken, a
fail-open decoy check and leases outliving cloud tokens, all fixed and tested. Measured again on 2026-11-14 (load about 20): values at 9,900 keys took 89–122 ms on SQLite and
155 ms on Postgres, within the noise of the Performance table; nothing to fix. Reviewed again on
2026-11-14: an independent security review of #39–#41 found racing rotations
could reset the login in use (now a claim never takes a busy role), a `#` in a password shown as
the host, Postgres connections outliving their timeout, and deleted projects leaving credentials
unrevoked (now revoked first, the rest retried by the sweeper); all fixed and tested. Earlier,
2026-11-13 (later): the Link dialog crashed on a kind that isn't a canvas
resource (a rotator, an AWS role): `KIND_ICON` only knows the workspace's. It names roll secrets'
own kinds now and falls back to a plain link icon for any it doesn't know. Earlier, 2026-11-13 (late): #39's own review found two ways a rotation could reset the
role in use (a refused `ALTER ROLE`, two environments sharing the pair); both fixed and tested.
The other threads' heads need nothing from roll secrets (MCP lists outside databases; previews
waits on #31). Earlier, on 2026-11-13 (night): the core and the stacked pull requests are level with
Doppler, Infisical, Vault and 1Password (clients, IaC, CI, Kubernetes, BYOK). The gap that's left
is Vault's signature: dynamic credentials for clouds and databases roll doesn't run. CI stays
under a minute (the new `sdk` and `terraform` jobs take 39 s and 22 s).

Ranked (twenty-ninth review, 2026-11-17 morning, against Vault, Infisical, Doppler, AWS Secrets
Manager, 1Password, Akeyless and step-ca, and the other threads' branches): first what can be built
now, then what waits on a merge or another thread. Since the twenty-eighth: #68 landed; the preview
hole is closed on both sides (#151 with previews' #160); the gateway (#166) covers every outside
engine, a second node, identity logins and revocation at once, and opens a stream in ~1 ms; MongoDB
Atlas (#173), outside search and CQL engines (#165), TOTP keys (#194) and guard scans (#169) are in
PRs; a screenshot pass fixed four dashboard bugs; the read path measured unchanged by #151. The
bottleneck is no longer building: about thirty green pull requests wait on a lock five threads share,
so landing them in batches comes first. Reading roll apps' #191 found that values from roll secrets
not tagged `sensitive` still reach an app's log; preview service logs aren't masked at all (both
asked of their threads).

On main: #20, #62, #63 (the CSI provider), #65 (conditional reads and client caches), #85
(roll-access's tokens), #74 (live links' digests), #66 (Terraform without values in state),
#67 (transit in the Go and Python clients, 2026-11-17 as 0fa832ddf), #151 (previews follow
their branches, 2026-11-17 as 48868a1e2; with roll previews' #160, fdffd596f, a preview's every
followed key, renamed or not, gets its branch's value or a blank: the hole the twenty-eighth review
found is closed, checked live), #68 (AWS through roll's issuer; roles and users edited in
place, 2026-11-17 as 3f829ca42) and #198 (plain link fields, 2026-11-17 as bc805ae41: roll apps
hides every value from roll secrets in its logs unless it's plain connection detail).
On main since 2026-11-18: #69–#73, #75–#77 and #147 (batch 1); #132, #136, #144, #165, #166, #173 and
roll redis's #375 (batch 3, 2026-11-19); #79, #81–#83, #87, #88, #92, #95,
#97, #102, #105, #110, #113, #117, #121, #124, #128, #145, #169, #194, #212, #213, #227, #229 (batch 2).
In pull requests to main, each green: #79 with #95,
#81 (read quotas across the fleet), #82 (metrics), #83 (scheduled backups), #86 (rotation
windows), #87 (encryption keys by name), #88 (password policies), #92 (SaaS rotators, the
credential checked at registration), #97 (search keys say `access: data`), #102 (Kubernetes
service-account tokens), #105 (Java), #110 (.NET with `IConfiguration`), #113 (an AWS Lambda
extension that logs in with the function's role), #117 (an Ansible collection), #132 (LDAP and
Active Directory), #136 (Snowflake, on #132), #144 (ACME on project CAs), #145 (Pulumi), #165 (Elasticsearch, OpenSearch, Cassandra and ScyllaDB
outside roll, on #132), #166 (a gateway into private networks, on #132), #169 (a credential
guard's scans audited as `secrets.guard`, for roll MCP), #173 (MongoDB Atlas, on #136), #194
(TOTP keys).

**State, 2026-11-18 13:25.** Built today, each green and mergeable: #248 (the polling path), #262
(Terraform for batch 1's kinds), #264 (doc fixes), #274 (queue links), #276 (a split deployment key),
#277 (secret requests), #279 (queues' default-exchange permission), #281 (a tamper-evident audit log),
#284 (six more import sources), #290 (AWS IAM login), #303 (credentials roll doesn't hold yet); and
on the #132 stack, #165/#166's engines through the gateway, their `rotate-admin`, and Cassandra's
cache waited out. Ranked now:
1. **The thirty-first security review** (2026-11-18 afternoon, today's PRs read cold): one high, three
   medium, six low, all being fixed (#277 4a23eabc2: answers stored literally, a per-request attempt
   count in its row, replays refused; #303 e4be05f1c: block fails closed; #304 3b1757d45; #290 6dd730cf8: a signed
   login used once, a server ID derived from the deployment key; #262 217be1314: the rotator's `url_wo`;
   #274 e1e2a89e7: a user's URL built from parts; #248 2236e55d1: an LRU, not a clear-all; #281 78e1c7c9d: only keys whose sealed half opens under the
   deployment key sign, verify or reach the JWKS, a keyed MAC on the head, hourly digests with a
   `stale` check, `verify --file` needs `--jwks`). All the findings are fixed. High: #281's verify (and the public JWKS) trusted any key a
   database writer added; now only keys whose sealed half opens under the deployment key. Medium:
   #281's head forgeable after a tail cut (a keyed MAC and hourly digests with a staleness check);
   #277's answer `${production.X}` became a reference (stored literally); #303's block mode failed open
   when the lookup errored. Low: #290 replay and a shared default server ID, #262's rotator URL in state,
   #274's ignored URL setters, #281's offline verify, #277's limiter reset, #248's cache clear-all.
   Sound: #276, #279, #284 (one stderr note).
2. **Batch 2 landed** (2026-11-18, one lock turn, main 49a6f1d8c): 24 PRs in four queue rounds, #79–#229,
   #81–#213, #92–#212, #117–#128; main's tree after each round was exactly the simulated one, and
   after the last the exact tree re-gated on the day's main (3feff202b). Batch 3 is ready (2026-11-18
   evening): #132; #136 + #165; #173; #166; #144 in five queue rounds, levelled, each gated, the
   combined tree gated on main c59fca59c with the live outside-databases check (`batch-3.txt`,
   `batch3-round.sh`). **Batch 3 landed** (2026-11-19 02:56–04:07, one lock turn): #132; #136 + #165;
   #173; #166; #144 (main cd65c5fcb = the gated tree plus #375), with roll redis's auth-bypass fix #375
   ahead of it in the same turn. Batch 4 (preparing, 2026-11-19 04:15): today's
   PRs (#248 #262 #264 #274 #276 #277 #279 #281 #284 #290 #303 #304 #329 #330, the issuer work) and
   #86 (`batch-4-notes.txt`).
3. **R2, cut off everything that reaches a folder** (#304): tokens through roll-access's bulk revoke,
   identities disabled, leases ended, shares and exports cut, syncs paused, an optional freeze (423).
   Fixed (3b1757d45): a project admin could revoke the operator's all-projects tokens (a cross-project
   denial of service); now only grants whose every rule names this project are revoked, the rest are
   listed as `skipped` ("the operator revokes it"); the operator and roll admins revoke all.
4. **The dashboard's "Store in roll"** for #303's notices (#330, on #303): roll reads the credential
   from the change's own file and stores it in the project trusted for the repo (a writer on the repo
   with `write` on the folder; never overwrites, never returns the value); it found #303 naming the
   wrong line past blank context (fixed on both). And **R5, `roll://`
   wherever another roll product takes a key** (#329): `Secrets::resolve_for(resource, roll://…)`, the
   project from the resource's trusted canvas (a reference may only name that one), read as that
   project's runtime (an untrusted ephemeral environment never reaches a protected one; dynamic
   credentials refused), audited as the resource; `digest_for` to notice a rotation. Storage's
   replication targets take references first (resolved per client, rebuilt when the digest moves;
   operator-only to set, for now); `ResourceRef::untrusted()` (04b3e7ddb) narrows a branch or copy to
   an untrusted read (no protected environment, no sensitive key; the digest refuses the same).
   Next: let whoever may `reveal` the key set the reference, then
   search's snapshot repositories, the reviewer's provider keys and the gateway's upstreams, each
   with its thread.
4b. **roll's issuer for the other products** (#357, on #281; asked by roll apps): an in-process
   `Secrets::issue(subject, aud, ttl)` for apps' per-instance workload tokens (`roll-apps:<project>:<service>:<env>`,
   Vercel's OIDC federation), scheduled key rotation (publish ahead, retire after the last token
   expires), and secrets' own workload logins trusting roll's issuer for apps' subjects; one issuer,
   never a second one. Built: 90-day rotation (published 2 days ahead, old key kept 2 h after the
   switch, retired keys kept sealed so old audit digests still verify), one audit row per subject,
   audience and hour, `ForApps::issue` behind the same trust check as `resolve`.
5. **A sealed start for the deployment key** (#367, on #276): `ROLL_SECRET_KEY_SEALED=<split>:<threshold>`
   starts rolld with only `POST /v1/secrets/unseal` up; K shares from the split, checked against the
   store's key id, start every product with the key in memory (all eight readers take it as a value);
   10 shares a minute per address, 5 failed sets an hour per split (durable); `roll secrets unseal
   --node …` for a fleet, each node unsealing itself. The thirty-second security
   review (2026-11-18 evening): one high, two medium, four low, all being fixed. High: #304's freeze
   let roll's own products through (every in-process reader counts as the folder's admin), so a
   frozen folder still fed previews, checks and apps; only people, the operator and roll-access
   grants pass now (#304 14e7e55ab: a private `frozen` flag only the operator's, a person's and a
   token's constructors set; tested for apps' deploys and untrusted checks). Medium: #281's old head replayed after a cut (4b4fe4033: the sweeper signs a
   `gap {after, since}` digest when the chain's newest digest is stale or isn't the head's; verify
   reports it from then on); #277's
   replay refusal beaten by changing the box's `v`. Low: #330 storing an outsider's credential, #290's
   duplicated `Signature=`, #329's source bucket choosing the reference's project (and its refusal
   text an oracle), #248's per-reader eviction. Sound: #303's fixes, #274, #262, #281's key provenance.
5b. **The thirty-first product review** (2026-11-18 night): the store is level with or ahead of the
   category; what's left is the first ten minutes, two test gaps and three products' own credential
   copies. Ranked:
   - P1–P4, **the first ten minutes** (#370: a saved login's URL by default, approvals printed and
     resent with `X-Roll-Approval`, the quick start and dashboard dead ends fixed): the CLI
     keeps 127.0.0.1:7070 after `roll login`; the first `roll run` waits silently on an `ask` approval
     it can't handle; the quick start has no install or login; the dashboard's commands dead-end.
     Then a committed `[secrets]` in `roll.toml` and an interactive `setup` (Doppler's `doppler.yaml`),
     designed (2026-11-19) so a repo suggests and the person approves, as direnv's `allow` (building:
     `feature/roll-secrets-repo-setup`):
     - `roll.toml` may hold `[secrets] project = "shop"`, `environment = "development"`, `path`, and
       `branches = { "release/*" = "staging" }`; never an API URL, a token or a value (the #58 lesson:
       a repository never chooses where a token goes).
     - It's only a suggestion. `roll run` uses it once the person has approved that exact table for
       that directory (`roll secrets setup` with no flags shows it and asks; the approval, with a hash
       of the table, goes in `~/.config/roll/secrets.json`, so a changed table asks again). Until then
       `roll run` says what the repo suggests and how to approve it, and uses nothing.
     - A protected environment is never taken from the file, even approved: `-e`, `ROLL_ENV` or the
       person's own setup only (a cloned repo can't route production values to code it ships).
     - Precedence: flags, then env vars, then the person's own setup, then the approved table, then
       `development` and `/`. `doctor` names which one was used.
     - `setup` in a terminal picks from the projects and environments the login can see.
   - P5 + P14 (#365): CI snippets that work on a fresh runner (the GitHub Action, or the install step
     plus `roll run`; GitLab's `id_tokens`; Azure's install step) and the write-flow component tests
     (a pasted `.env` as one change, a protected edit becoming a change request, a stale edit reloading).
   - P6, MCP tools: import, rollback (destructive), create environment, link and link targets in roll
     MCP's #353 (rollback's `bypass` left out); secret requests and cut-off when batch 4 lands; the
     development preset change (#354) was closed after roll MCP's review: `roll login` is an ordinary
     OAuth client, so a looser preset would loosen every approved agent too. The first run is fixed in
     the CLI instead (the approval link at once, then a remembered "allow from now on"); remembered
     approvals are already per environment and folder and tied to the asking client (revocable on the
     token page); roll MCP adds 7- and 30-day choices next.
   - P7–P8, **tests** (building: `feature/roll-secrets-test-gaps`): `roll run --watch`, the six links
     to roll's own products, and a `pnpm test:secrets` gate.
     The links test found **an authentication bypass in roll redis**: `AUTH default <db>_<anything>`
     answered +OK (a `nopass` default user behind the platform-secret check), so any password, and any
     rotated-out one, logged in. Fixed in #375 (a hosted database's default user
     has no password, never `nopass`; a rotated-out secret refused at the next AUTH), landing ahead of
     batch 3 in its own lock turn.
   - P9–P11, apps' variables, search's connectors and previews' variables as `roll://` references
     (each with its thread; search's is building on #329).
   - P12 members (R1, waits); P13 a CI token's defaults (all environments, no expiry: the user's call);
     P14 a dashboard write-flow test; P15 R4 and real-account checks.
   - Inconsistent: `roll://` is also the desktop app's deep-link scheme (`roll://agents/…` parses as a
     reference; proposed: the desktop app moves to a scheme of its own, e.g. `roll-app://`, since
     `roll://ENV/KEY` references are in the CLI, VS Code, the clients and #329 already; the agents
     owner's call), `doctor` suggests `export > .env`, GitLab is listed without a snippet.
6. **R10, decoy keys** (#392, gated green; merges main after batch 4, re-gated, batch 5): a key
   holding a decoy `rlt_` token (#270), minted in process by `Access::mint_decoy`, alerting
   `decoy_used` (audit, notify URL, top health finding; at most hourly) through `Access::on_decoy`. Only
   admins see it's a decoy. A Postgres decoy role needs a login hook in pg-proxy, designed in #392's
   body. IAM users need an account;
   **R4** (GCP KMS, Azure Key Vault, HSM) and confirming #290, #284, #227, #229 on real accounts: the
   user's call.

**The thirtieth product review** (2026-11-18, against Vault/OpenBao, Infisical, Doppler, AWS, 1Password,
Akeyless, GitGuardian and Bitwarden SM, open PRs checked first) ranked these gaps; its four doc
inconsistencies are fixed in #264:
- R1. **Project members and roles for people** (L; every competitor's unit of sharing): waits on the
  accounts/workspace and roll-access threads.
- R2. **Cut off everything that reaches a folder** (M): reach lists it but can't act; one action to
  revoke its tokens and identities, end leases and shares, pause syncs and optionally freeze reads.
  roll MCP's #267 (on main) has `Access::revoke_many(ids, why, by)` (atomic, ≤500 ids, one audit row
  per grant). Building: `feature/roll-secrets-cut-off`;
  reach passes its own grant ids, and leases, shares and syncs stay in secrets' transaction.
- R3. **Credentials roll doesn't hold yet** (#303): a landing's added lines run through
  `roll_auth::credentials` with the stored-value check; a match roll doesn't hold is a `secret_unstored`
  notice naming kind, `path:line` and the `roll secrets scan --store` command (never the value), or a
  blocker with `roll.toml` `[secrets] unstored_credentials = "block"` (admin bypass audited);
  placeholders (`example`, `fake`, `test`… parts, runs of one character) and `.roll/secrets-allow`
  (fingerprints, path globs) are skipped. `scan --store` moves one into roll and prints the env read.
  Next: the dashboard's "Store in roll" (roll reads the value from the change's blob).
- R4. **GCP KMS, Azure Key Vault and HSM keys** (M), next to AWS KMS: real accounts (the user's call).
- R5. **`roll://` wherever another roll product takes a key** (M): agent endpoints (they take `op://`),
  the gateway's upstreams, review provider keys, storage replication and search snapshot credentials.
- R6. **roll queues as a link provider** (#274): live host, port and vhost; an AMQP URL with a user
  of its own, managed or dynamic, and only the access asked for (read, write, configure). It found
  roll queues checking a default-exchange publish against `""` rather than `amq.default`, fixed
  for every protocol in one place as RabbitMQ does (#279, checked against real RabbitMQ). roll apps
  wires queue links into `RollResources` once #274 is on main: tell them when it lands.
- R7. **AWS IAM login for EC2/ECS/EKS** (#290): a signed `GetCallerIdentity` the server sends only to
  AWS's own STS hosts (URL rebuilt, body exact, `X-Roll-Server-ID` signed, ±5 min), matched against
  one role, session prefix or user; CLI, Go and Python sign from the standard credential chain
  (IRSA, ECS/Pod Identity, IMDSv2). Unconfirmed against real STS: an account (the user's call).
- R8. **A tamper-evident audit log** (#281): each row hashed onto a per-project chain in its own
  transaction (one head row, a unique seq: no forks, checked with two racing nodes), hourly digests
  signed by roll's issuer key and sent to the sinks, `audit verify` online or offline against an
  export and a saved JWKS; retention cuts only at a signed digest. ~+0.1 ms per audited write on
  SQLite, ~one round trip on Postgres with one writer. Under many readers of one project (a
  500-pod rollout) the chain alone capped reads at ~500 a second with p99s of seconds, left reads
  unaudited and failed most changes; reads are now group-committed per node and queue across nodes
  on a claim, and a change that loses once runs next holding it ("Tamper evidence", "Under load").
  Known: rows cut from the end along with the head, after the
  last delivered digest (≤1 h), aren't detectable without a sink.
- R9. **Secret requests** (#277): `roll secrets request KEY…` or "Request a value…" makes a link;
  the answerer's browser seals each value to the request's own key, roll opens it only in the write
  that stores it (a change, or a change request the requester can't approve), once, rate-limited,
  audited. Screenshots in `~/.local/share/roll-review/secrets/requests/`.
- R10. **Honeytokens** (M): decoy `rlt_` tokens are roll-access's (roll MCP's #270, on main);
  database roles and IAM users that alert on use are this side's, once those land.
- R11. **Moving in from 1Password, Bitwarden SM, Parameter Store, GCP, Azure and Akeyless** (#284):
  each through its own CLI with the user's login, bounded (500 secrets, 16 MiB, 60 s), stopping on an
  unreadable secret or a collision, `--dry-run` for every source, `--sensitive` opt-in for
  SecureStrings. Shapes from vendor docs only (Akeyless least sure): real accounts are the user's call.
- R12. **A split deployment key** (#276): `roll secrets key split/combine`, Shamir N of M in a small
  I/O-free crate (`roll-keyshares`), checksummed `rlks_` shares with a set id and a key check (a
  wrong mix is an error, never a wrong key); backup keys split the same way. Next: a Vault-style
  sealed start (`ROLL_SECRET_KEY_SEALED`, holders unseal over the API), designed for fleets. R13. **A FIPS build** (L).
  R14. **Format-preserving encryption** (M, low). R15. Clearing a copied value from the clipboard: not
  done, since a page can't read the clipboard without a prompt and clearing blindly could wipe
  what was copied since.

0. **Land in batches**: batch 1 landed in one lock turn (2026-11-18): #69, #70, #72, #76, #77, then
   #73, #75, #71 (main's tree afterwards was exactly the gated one), and #147 behind them, gated on
   their tree. One flake on the way (a CI checkout hung past its 2-minute limit; #71 re-queued after a
   re-simulation of the new order). Next: batch 2 the same way, prepared and gated before the next
   lock turn: ready (2026-11-18 07:20): 24 PRs in four queue rounds (`batch-2.txt`, `batch2-round.sh`),
   every one levelled with main and the combined tree gated on every step (Rust on SQLite and
   Postgres, access, CLI, dashboard, and the Go, Terraform, Python, Ansible, Ruby, Java and .NET
   clients); logged in the merge order after search's next turn, waiter `rs79-lock`. Then batch 3:
   the #132 stack in four rounds (`batch-3.txt`; #166 last, after #173), with #144, #86 and #248.
0a. **The thirtieth independent security review** (2026-11-17, tonight's PRs #132, #136, #144,
   #165, #166, #173, #194, #227, #229, read cold): no high finding; ACME's JWS, nonces, EAB and
   tls-alpn-01 verifier, the gateway's allow-list, bounds and revocation, and every PR's network
   guards held. One medium: a gateway's link could be opened with a person's session (a WebSocket
   GET has no CSRF check, and the session was checked once) — now only the gateway's own token or
   login opens it (#166). One low: an Atlas user named `.` or `..` turned the API path into the
   collection's (#173, refused now). One to confirm: ScyllaDB's audit log with a password in a CQL
   statement: answered (#165, d8d21b7c7). ScyllaDB's audit log masks a plain password, but tracing
   keeps the whole statement on both servers, so rolld now sends `HASHED PASSWORD` (bcrypt, made in
   rolld) wherever the server takes it: always on Cassandra, and for a superuser's `CREATE ROLE` on
   ScyllaDB, whose `ALTER ROLE … HASHED PASSWORD` is a server error. Checked live on both.
0c. **Azure DevOps** (#227): a sync to variable groups as secret variables, and Azure Pipelines
   logging in with its own OIDC token (one pipeline or service connection per identity), as Vault,
   Infisical and Doppler integrate. Azure never returns a secret variable's value and its update
   rewrites the whole group, so the sync sends every value on each push and refuses a group holding
   secrets it doesn't manage. Unconfirmed against a real organization (the user's call).
0e. **Four more syncs** (#229): Supabase Edge Function secrets, Cloudflare Pages variables, Azure
   App Configuration and HashiCorp Vault / OpenBao KV v2 (the gradual way off Vault), as Infisical
   syncs to each; a `refuses` hook so a kind can refuse key names (Supabase's reserved prefix).
   Merges clean with #227. Unconfirmed against the real services (the user's call).
0d. **A Ruby client** (#212), the same surface as the TypeScript, Go and Python ones, both sealed-box
   vectors opening in it, standard library only, with Rails notes. Writing it found that Python's
   and Go's `Values` printed every value and their `Client` the token (#213 fixes both).
   Transit is now in the Java (#105, 5570f5b11) and .NET (#110, 7c91476c7) clients too, its errors
   and printing checked to hold no plaintext; next, TOTP for Java, .NET and Ruby once #194 is on main.
0b. **The polling path at scale** (#248): a 304 or an unchanged `digest` at 9,900 keys now takes
   0.1 ms on SQLite and 3 ms on Postgres (was 99 and 169 ms), so 1,000 pods polling every 30 s cost
   ~0.1 core, not ~5.6. A digest reads only the columns it hashes and each folder once, and each node
   keeps it while one read of the database's state (the project's change number, environments,
   imports, required keys, shared folders' projects) says the same, every permission it asked
   answers the same, and live links' generations and fingerprints hold. Next: the first digest after
   a write (~70–100 ms per polling identity) if a fleet's numbers call for it.
1. **Land the queue**, one at a time when the merge lock is free (the order is in "Landing"):
   four threads share it, so each gate runs before the lock is taken and only main's newest change
   is merged under it.
1a. **Measured after #151** (2026-11-17, 9,900 keys on SQLite, release, five alternating runs each way
   at load 25–68): every read's before and after ranges overlap, an ephemeral environment's values
   included (the path that loads its branch list); #151 adds one lookup per request, not per key.
1b. **A screenshot pass over the dashboard** (2026-11-17, #132, #136, #165, #166, #173): it found
   textareas one line tall (a fixed height beat `rows`; now `textareaField` everywhere, main's CA
   boxes too, on #144), cut-off labels, Active Directory's fields offered for an OpenLDAP directory
   (link targets now ask each resource for its fields), and a gateway's token refused on a server
   without sign-in (the operator now makes it, as `POST /v1/access/tokens` does). All fixed and
   carried up the stacks.
2. **Keeping the queue level with main** (done 2026-11-17 for all fifteen that conflicted, each
   pushed on its own checks; #76 now also marks syncs when a product's own route changes a
   credential, as #74 put that in `source_changed`). **Landing the stacks together where it saves a
   lock cycle** (#128 on #105, #124 on #102, #95 on
   #79, #117 on #121, #136 on #132), and the Spring integration's first CI run (#128).
3. **ACME on project CAs** (#144), as Vault's PKI and step-ca serve it: cert-manager, certbot,
   Caddy and lego get certificates from a project's CA with no roll client (lego and certbot checked
   against a local rolld). Accounts always bind an EAB key (looked up on every request, ended with
   its maker's reach); `dns-01` through public DNS, `http-01` through the guarded client, and
   pre-authorized names for clusters roll can't reach (off by default); `tls-alpn-01` and reused
   authorizations too (Let's Encrypt's behaviour cert-manager expects), and finalize re-checks the
   binding. Next on it: checking pre-authorized names through a gateway (item 6) once that's built.
4. **Active Directory, done against Samba AD** (#132): every step run against a real domain
   controller with a delegated bind account. It found AD's hour-long acceptance of a reset account's
   previous password, now closed by a throwaway reset first; logins also hand out the UPN and
   `sAMAccountName`. Left for a Windows Server: its password policies, AD CS certificates, LDAP
   signing and channel binding, replication between domain controllers (an account of the user's).
4b. **Elasticsearch, OpenSearch, Cassandra and ScyllaDB outside roll** (#165, on #132): Vault's
   database engine has them; two roles taking turns and a login per lease, each checked live
   (Elasticsearch 9.5.5, OpenSearch 3.9.0, Cassandra 5.0.9, ScyllaDB 2026.3.3). Cassandra's
   credentials cache let a dropped role in for ~1.6 s, so a lease's end sets a password nobody knows
   first. All four go through a gateway too (#166, checked against the real servers), and their admin's
   own password rotates (`rotate-admin`, #165: it fell through to Postgres's statement), waiting out
   Cassandra's ~2 s credentials cache; other schemes are refused by name. Managed rotations wait
   the same way (with an app logged in, Cassandra refused the handed-out password 12 of 12 times
   before, 0 after). Known: the cache is per node, and an ended lease's password can still log in
   for up to `credentials_validity` (documented; the servers' setting shortens it).
   Next: OpenSearch's narrower permission set, and Couchbase and Oracle (Vault has both).
4c. **MongoDB Atlas through its Admin API** (#173, on #136): Atlas refuses the user commands on
   every tier and rolls back changes made another way, so the MongoDB engine never could rotate an
   Atlas login (it now refuses Atlas hosts and points here). Users taking turns and a user per
   lease (`deleteAfterDate` as a backstop) through an organization service account, as Vault's
   Atlas plugin. Unconfirmed against a real account (the user's call).
4d. **TOTP codes from a stored seed** (#194), as Vault's TOTP
   engine and 1Password's one-time passwords: a `totp` key hands out its current code (`reveal`,
   audited, never the code in the row) to CI that logs into a 2FA-protected service; the CLI, the
   dashboard (a countdown) and the three clients.
5. **Terraform resources and MCP tools for the new kinds**. #262 (2026-11-18): rotators, audit sinks,
   GitHub Apps, AWS roles and users, cloud accounts, required keys and the KMS key, credentials
   write-only (never in plan or state, checked), and import for every registration. Then, as each lands: SaaS
   rotators, Kubernetes clusters, backup schedules, rotation windows, directories, Snowflake
   accounts, ACME bindings; the tools with roll MCP.
6. **A gateway into private networks** (#166, on #132; Infisical's Gateway, Akeyless'): a small process inside a
   VPC that dials out to roll and carries rotations, dynamic credentials, directory calls and ACME
   challenges to private addresses, so private databases need neither a public address nor
   `allow_private`, and pre-authorized ACME names can be checked after all. Its link is a
   workload identity; it carries only the conversations rolld starts, only to the hosts its own
   local allow-list names (a compromised roll can't scan the network through it), as bytes with TLS
   end to end; on a fleet its link and the calls through it run on one lease holder. Built: Postgres,
   MySQL, SQL Server, Redis, MongoDB and LDAP/AD through it, a real agent over a WebSocket in rolld's
   tests (allow-list, backpressure, revocation) and Postgres end to end; revocation takes effect at
   once (every stream open looks the token and record up, ~0.11 ms); a stream opens in ~1 ms, not
   44 (Nagle: `TCP_NODELAY` on every hop, rolld's API listener and roll-edge's included).
   ClickHouse and RabbitMQ go through it too (a hyper-util client whose connector opens the
   gateway's stream, TLS in rolld; checked by hand against real servers), `mongodb+srv://` is
   resolved by the gateway with the network's DNS and its own allow-list, and a two-node test covers
   the link following its lease, a call from the other node, the internal route's signature, and a
   drain's reconnect; a gateway logs in with a workload identity and renews with no stream lost;
   port ranges in its allow-list. Next: ACME's pre-authorized names checked through a gateway (once
   #144 and #166 are both on main).
7. **Pulumi** (#145): roll's Terraform provider through Pulumi's any-Terraform-provider package
   from a local build (no registry release), with TypeScript, Python and Go SDKs generated and an
   end-to-end `pulumi up`/`destroy` checked. Pulumi keeps every input in its state (encrypted), so
   values can't stay out of it as they do in Terraform. Next on it: a CI job, and publishing to
   the registries (public: the user's call).
8. **Access requests**: roll-access's `ask` rules already are Infisical's and 1Password's
   time-boxed requests for tokens and agents (a `reveal` held for an operator, approved once or
   remembered for 1h/8h/1d), so roll secrets builds no second system. #147 (on roll MCP's #114,
   which added `roll_auth::Approved`) records which approval let a request through, and who
   approved it, in the audit row and the activity timeline. Next: reveals in a protected environment
   needing two approvers, through roll-access's two-person policies (roll MCP is building them),
   matched to change requests' rule that an author never approves their own. People's own requests
   wait on projects having members.
9. *To check against the real services (accounts, the user's call):* #113's STS login
   (`GetWebIdentityToken`) in an AWS account, Entra's answer for removing a secret that's already
   gone (#92), #136's statements on a Snowflake account, and #173's Admin API calls on an Atlas
   project.
10. *With roll MCP:* their credential guard checks what an agent's tool call would write against
   `POST …/leaks` (only values the token may reveal; `sensitive` keys never, so it can't be an
   oracle), marked `X-Roll-Guard: 1` (#169 audits it apart). *Waits on roll MCP:* #114 (`roll-oidc`, approved here), the GitHub Action's `client-id` mode
   (their diff, reviewed here), and their `roll_machine_client` Terraform resource (their diff
   comes here first).
11. *roll search's #134* (inference keys from roll secrets): on main (e7d0389af, 2026-11-19); offered for
    services taking an `api_key` or Entra ID; Bedrock's access keys and Vertex's service account (several
    fields) are a later step. Re-reviewed and approved 2026-11-18 (a
    comment on the PR): product-made syncs need `reveal`, every push re-checks its maker, the pin is
    checked before and after `_update`. Follow-up: show "pushes as <maker>" and warn before it fails. Earlier: approved once its pin covers the endpoint's whole destination
   (`service_settings` but the key, rate limit and allocations; `task_settings` where they route).
11b. *roll search's #361* (snapshot repositories take `roll://` references; on #329): reviewed
    2026-11-19, blocked. A reference resolves as the cluster, and roll never sees who chose it. So an
    Elasticsearch admin credential (a native user or the `elastic` password, outside roll-access, alive after
    its holder leaves) can point an `s3` repository at its own endpoint with a production key as
    `access_key`. That key goes out in the SigV4 header. Asked of search: a per-cluster allowlist
    set through a `/v1/search/clusters/{id}/…` route by a caller that `Caller::allows(Reveal)` on
    each environment, with its maker stored and `maker_reaches` checked before every `resolve_for`.
    Also asked: bind the resolver to the cluster id, not its name; zeroize held values and drop them
    with the repository; give one message for protected and missing keys on branches; add a
    lost-access test through the real `ForSearch`.
12. *Waits on roll apps:* their redeploys holding `wait_digest` once #79 lands; #191's log
   redaction hiding every value that comes from roll secrets unless #198 marks it `plain`: done
   (apps' #201, a41ac171c), a URL's password hidden even in a plain value.
   *Waits on roll previews:* masking secret values in a preview service's logs and its jobs' output
   (#200 and #203, approved here: logs, job output, screenshot text, health bodies, egress names;
   their egress proxy refuses before resolving, and a fork's preview now defaults to Roll-only
   egress with a DNS server that resolves nothing outside).
13. *Waits on other threads:* a live check of OpenSearch links, roll redis and nosql keeping a
   rotated password a while, splitting the CLI's `secrets.rs` once the queue has landed.
14. *Waits on linking chat users to roll accounts:* approving change requests from Slack.
15. *Waits on products serving signed-in people:* a person's project linking their own databases,
   buckets and streams.

### Landing

**To main** (merges are approved from 2026-11-15; one thread at a time behind
`~/.local/share/roll-merge/lock`). About 50 pull requests sit on #20; the plan folds them into a
few reviewable ones and closes what they supersede:

1. **#20 → main**: merged 2026-11-15 as 1ad248420 (through main's merge queue: add a pull
   request with GraphQL's `enqueuePullRequest`, since auto-merge is off), after the full gate
   including `pnpm test:code --docker`.
2. **#62 → main**: retargeted once #20 is in. It is #31–#61 merged in order with every
   conflict resolved and seventeen independent reviews' fixes, so it lands as one squash
   (decided 2026-11-15: splitting it would redo conflict resolutions #62 already settled), and
   #31–#61 close as superseded by it (their branches stay; #35 and #59, which conflict on their
   own, are in it too). #30 closes as superseded by #64.
3. **The follow-ups, one at a time**, each a small pull request of its own: each merges main,
   is retargeted to main and lands in order — #63 (CSI provider), #65 (conditional reads),
   then #74 (live digests, after #65), #66, #67, #68, #69, #70, #71 (#70 and #71 together:
   see the merge note below), #72, #73, #75, #77 (after #75), #76, #79 (after #74 and #76).
   Pairs that touch the same lines and keep both sides: #73 with #75, #66 with #72, #65 with
   #67, #74 with #76 (`source_changed` calls `due_for_source` in its write).
4. **#64** (the landed stack on roll-access): rebuilt on main once #62 is in (#30's change and
   its two follow-up commits), not merged: its branch predates the squashes of #24 and #25.

**Merging main after a squash.** A squash leaves main without the branch's commits, so merging
main into the next branch would conflict wherever it changed the squashed lines. Once main has
squash S of tip T: check `git diff S T` is empty, merge `S^` normally, then `git merge -s ours S`,
then anything after S normally. Before `-s ours` on a follow-up, check that it contains the
squashed tip (`git merge-base --is-ancestor <tip> <follow-up>`); if not, merge the tip normally
first, or `-s ours` would silently drop the commits it lacks. The same goes for #74 after #65. No base branch is deleted
until the pull requests on it are retargeted (deleting one closes them).

Before each lands: origin/main merged, the full gate (`cargo build/test/clippy`, roll-secrets on
Postgres, `pnpm typecheck` and the dashboard's tests, the SDK suites, `pnpm test:code --docker`
where AGENTS.md asks for it), CI green, a squash titled like #18–#27, main's CI green on the
merge commit, a line in the merge log.

Merging the stack in order (2026-11-14) found what no single pull request could:

- **Customer-managed keys (#37) and the providers built beside it.** Databases outside roll (#39),
  CAs (#44) and SSH CAs (#45) opened a project's data key with the deployment key alone, so their
  links failed on a project whose key a customer's KMS wraps. They unwrap first, as rotators do.
- **`roll run`**, which four pull requests changed: the fallback (#35), references in env files
  (#49), masking (#54) and the strict read (#55) now work together. A run started from the
  fallback follows a digest no folder has, so the first answer from rolld reloads it, and leases
  are kept as their renew routes.
- **Relogin (#56) and the fallback (#35)** both changed the API client: a 401 for a workload's
  login logs in again inside `answer`, which the fallback reads through.
- **Lines three pull requests changed alike** kept only the last one's version: health's finding kinds
  lost `rotation_failing`, `revoke_failing` (#48) and `certificate_expiring` (#44), and the
  registries a forgotten project drops lost `cloud_accounts` (#43). Every pull request's added lines
  were checked against the landing branch afterwards; everything else is there or extended.
- Additive everywhere else: routes, catalogue rows, `Options` fields, types and this document.

A nineteenth independent review (2026-11-15, of #62 with main merged, #73–#79) found two
high-severity issues: two admin rotations at once could still lose the only working password
(#75: a rotation in flight holds others off for 5 minutes, a promote that misses keeps whichever
URL logs in, PATCH waits for it, and a rebuilt URL must keep its login and host) and `roll run
--watch` spinning when a long poll failed fast (#79: a failure waits the interval; the server
holds at most 25 s and 32 polls per principal; a stopped watch abandons its poll). Also fixed:
one key's reveal in an untrusted preview could issue a dynamic credential (#62: the rule is the
resolver's now), signing secrets shown to a grant without `reveal` (#62, #77), a rotator moved
while its retired keys still wait to be revoked through it (#77). Merging #25 into #62 needed
`roll_auth::Grant`'s new `user` field in three places.

An eighteenth independent review (2026-11-15, of #62, #73–#76 and the last fixes against the
other threads' heads) found two high-severity issues, both fixed on their pull requests: an
interrupted admin rotation could lose the only working password (#75: whichever URL logs in is
kept, every change a compare-and-swap, an interrupted rotation finished on the next use), and code
nobody vetted could get a GitHub App token with the App's full power (#62: untrusted callers get no
dynamic credential at all; #73: a registration sets the most a token may reach). Also fixed: a
registration moved to another server with logins still on the old one, Redis' shared default user
rotated (#75), a reader renewing another's lease (#66), a failed product read moving every live
digest (#74), the 2-second sync pass scanning everything under steady writes (#76), restart
failures turning off the operator's cheap path (#69), and an early renewal failing a call while
the token still worked (#62). roll previews resolves untrusted previews without dynamic keys
again (#28), and roll apps keeps one `roll completions` command (alias `completion`). Merging
#74 with #76: `Secrets::source_changed` calls `syncs::due_for_source` in its write.

A seventeenth independent review (2026-11-15, of #62 and #63–#72 against the other threads' heads)
found four high-severity issues, each fixed on its pull request:
- roll apps deploying a fork's preview got its unprotected base's sensitive keys: an untrusted
  ephemeral environment kept the runtime flag (#62: it's read as a non-runtime by everyone, and
  `ForApps` reads one as previews do, leaving out dynamic keys and what doesn't resolve);
- a reveal token could end every lease in its environment (#66: leases record who they were
  issued to; a reader ends only its own);
- a 304 could hide a password its product changed itself (#65: never a 304 for a folder that reads
  a live link, until products report their own rotations);
- the Go and Python clients never logged a workload identity in again, so a watch failed after
  15 minutes (#62: they renew before the token ends and on a 401, reading the OIDC token afresh).

Also fixed: every preview converge writing an audit row (#62), the operator stopping at the first
failed restart and letting a RollSecret's author restart anything in its namespace (#69: opt-in
per workload, every target tried, failures retried), a write and an audit scan on every 304 (#65),
the CSI provider logging in on every mount (#63: a login reused per service account token), and
every node checking the same oldest syncs (#62: each claims its own). For the other threads: roll
MCP's secrets tool descriptions have drifted (seven database engines, rotation of any managed
link, health's newer finding kinds), and roll search should merge before #62, since the landing
branch's in-tree search issues admin keys until it echoes `access`.

A sixteenth independent review (2026-11-15, of #63–#72) found three high-severity bugs, all
fixed on their pull requests: a watch on a caching client delivering old values and stopping
(#65: watches read fresh, answers with problems aren't kept, and a 304 records one
`secrets.revalidate` row a day so health and insights still see the reads), the maker chain stopping
at tokens (#64: resolved through roll-access, and reach judges tokens as their requests are), and
Terraform refusing values roll normalises (#72, with encryption keys now protected from destroy). Also
fixed: restarts on unchanged data (#69), leases a read token couldn't end (#66), a decoy check
passed by throttling (#68), the CSI provider's API over plain HTTP or any host (#63), a signing
secret printed for SIEM sinks (#70) and system indices for search sinks (#71). Merging #70 and #71
together: both add `with_project` to rolld's `syncs.rs` (keep one) and change the stream form's
`urlOk` (keep #71's targets with #70's formats).

A fifteenth independent review (2026-11-14, against Doppler, Infisical, Vault, 1Password, AWS
Secrets Manager and Akeyless, and the other threads' heads) found two high-severity issues, both
fixed: roll apps showed sensitive values to whoever could reveal (`ForApps` never set
`sensitive`), and the operator logged every RollSecret in as itself, so creating one in a
namespace could reach the operator's identity or another team's (now a namespace's own service
account outside the operator's namespaces). Also fixed: client addresses taken from the
client-written `X-Forwarded-For` entry (and MCP's calls audited without one), live rotations
the digest didn't see, resolves that couldn't refuse dynamic credentials (previews), search
keys a build made admin, a sync sweep that could outlast its interval, failed pushes audited as
if nothing went out. The rest of its ranking is above; the other threads have its findings on
their side.

A fourteenth independent review (2026-11-14, of the landing merges) found no lost behaviour in the
store, the providers, the values route or the catalogue; it found where features meet: a workload
identity's `roll run --fallback --watch` started through an outage never logging in again (#35 with
#56: the login is now pending until rolld answers, and a failing or timed-out login counts as no
answer), masking concealing an env file's own plain values (#54 with #49: only values from roll),
a strict read failing on a required key that is sensitive or dynamic (#55 with #46), a reference to
a sensitive key reported as unset (#49 with #46), a reference's read with no answer not falling back
(#35 with #49), and docs and comments merged twice or misplaced: all fixed, tested where a test
guards behaviour.

The landing branch passes roll-secrets' tests (SQLite and Postgres), roll-cli's, rolld's auth and
secrets tests, roll-access's, clippy with `-D warnings`, `pnpm typecheck`, the dashboard's 719
tests, `@roll/secrets`' and the VS Code extension's.

Open pull requests after this one:

- **#63, a Secrets Store CSI Driver provider** (after #62): `sdk/csi-provider`, a pod's volume
  gets an environment's values as files with no Secret in etcd, each pod logged in with its own
  service account; the driver's gRPC served with Go's standard library.

- **#30, one permission system** (after #20 and #25): a project's tokens are roll-access grants,
  identities' logins get `workload` tokens, `rlst_` is gone.
- **#31, roll apps' deploys get their project's secrets** (after #20 and #26): `ForApps`, linked
  keys marked for apps to rewrite, `may_reveal` by secrets' own rules (`Secrets::caller_of`).
  #30 and #31 both change `caller()`: whichever lands second keeps #30's (no `rlst_`) with
  #31's `caller_of` split.
- **#32, Go and Python clients** (after #20): `sdk/go/secrets`, `sdk/python/roll_secrets`.
- **#33, a Terraform provider** (after #20): environments, keys (a value or a link), identities,
  syncs, tokens and values.
- **#34, a GitHub Action** (after #20): a job's environment through its own OIDC token, masked;
  `scan: true` fails a pull request that adds a stored value.
- **#35, `roll run --fallback`** (after #20): an encrypted copy of the last values, used through an
  outage.
- **#36, a Kubernetes operator** (after #32): RollSecrets pulled with the cluster's own identity,
  and its Helm chart.
- **#37, customer-managed keys** (after #20): a project's data key wrapped by the customer's KMS key,
  set from the project's settings.
- **#38, dynamic AWS credentials** (after #20): a session per lease from a role the project registers.
- **#40, AWS IAM users' access keys** (after #38): two key slots take turns, the old key deleted
  after its grace; a user with two keys already is refused, never pruned.
- **#55, pulling into Kubernetes with External Secrets Operator** (after #20): its webhook
  provider (one store for whole environments and single keys) and `values?dynamic=false`.
- **#61, reach** (after #20): who and what can read a folder or key; `GET …/reach`, `roll secrets
  reach`, a key's "Who can read it…".
- **#60, server credentials under browser-bundle names** (after #20): `NEXT_PUBLIC_`, `VITE_`,
  `PUBLIC_`… holding a server-only value; the write warns, health's `public` finding.
- **#59, `roll completions` and direnv** (after #20): completion scripts for every command; a
  documented `.envrc`.
- **#58, roll secrets for VS Code** (after #20): hovers, warnings and completion where code reads
  a variable; `.env` values replaced by `roll://` references or moved into roll first.
- **#57, workload identities pin their issuer's keys** (after #20): `jwks` on an identity, for a
  cluster's own issuer; `identities add --jwks`, the dashboard's Other OIDC form.
- **#56, a workload identity's login renews itself** (after #20): long-running `roll run
  --watch` in a pod logs in again before its login ends, reading the rotated token file.
- **#54, `roll run` masks values in the command's output** (after #20): stdout and stderr that
  aren't a terminal go through roll, which writes `<concealed by roll>` for each value it added
  (as is, by line, JSON- and URL-encoded); `--mask`, `--no-mask`.
- **#53, change policies per folder** (after #20): a folder of an unprotected environment takes
  changes only through approved requests, with its own approvals and approvers.
- **#52, moving in from Vault trees and AWS Secrets Manager** (after #20): `import --from vault
  --recursive` into folders, `--from aws` through the user's aws CLI.
- **#51, search links on OpenSearch clusters** (after #20): an internal user (`user`,
  `password`) where the cluster has no API keys; the wrong field for a flavor is refused when linked.
- **#50, check-and-set writes** (after #20): `if_version` on a set or delete, refused as `stale`
  when the key moved; the dashboard's editor sends it.
- **#49, `roll://` references in files** (after #20): `roll run --env-file` and `render` resolve
  `roll://ENV/KEY` beside plain values, as 1Password's `op://`.
- **#48, failing rotations in health** (after #20): `rotation_failing` and `revoke_failing`
  findings, with the reason, until roll succeeds; a rotations overview of every managed credential.
- **#47, encryption keys** (after #20): encryption as a service, as Vault's Transit: named,
  versioned keys that never leave roll; encrypt, decrypt (audited), rewrap, data keys, Ed25519
  signatures and HMACs; keys brought from outside, Vault's ciphertexts included.
- **#46, sensitive keys** (after #20): a key tagged `sensitive` reaches only roll's runtimes, syncs
  and workload identities, never a person or a token; the tag is for good.
- **#45, SSH certificates** (after #44): a project's SSH CA signs short-lived user certificates
  for the logins it allows, and `roll ssh` uses them for one session; OpenSSH's formats, checked
  with `ssh-keygen` and a real `sshd` login.
- **#44, a project's own certificate authority** (after #20): short-lived TLS certificates for the
  names its name-constrained CA allows, dynamic or managed, and CSRs it signs; revocation and a
  public CRL; checked with a real TLS handshake and `openssl verify -crl_check`.
- **#43, dynamic GCP and Azure credentials** (after #20): roll as an OIDC issuer
  (`/v1/secrets/oidc`); GCP's workload identity federation and Azure's federated credentials trust
  its token for one project, checked with a decoy project at registration.
- **#42, MongoDB outside roll** (after #41): rotation and a user per lease for replica sets (not
  Atlas, which refuses the user commands on every tier: "MongoDB Atlas" uses its Admin API); the primary only among the seeds, SRV targets inside their domain, SCRAM with the server's
  signature checked.
- **#41, Redis outside roll** (after #39): two ACL users take turns (`#<sha256>` passwords), or a
  user per lease copying a template user's rules; the tenant's replies read by a bounded parser.
- **#39, Postgres, MySQL, SQL Server, ClickHouse and RabbitMQ outside roll** (after #20): two login roles take turns (rotation), or a
  login per lease in a group role (dynamic), over verified TLS to a public address rolld resolved
  itself (Postgres gets a SCRAM verifier, never the password).

Done since the last review: roll search's merge (#22: its links checked live, the conflicts additive); signed-in people manage the secrets of their own projects (People,
under Access), with the canvas guards. main (roll MCP #19 and accounts #18) merged in. roll secrets asks the
caller's roll-access Grant (`GrantGrants`): a grant's request is checked inside the product too,
health and leaks filter themselves, search and findings take any grant, ephemeral environments are
roll-access's `preview` class, and a sync made by a grant pushes only while the grant may.
