# The roll Terraform provider

roll secrets as code (docs/secrets.md, "Terraform"): a project's environments, its keys (a value,
or a link to a roll resource on the project's canvas) and, as a data source, an environment's
values for another provider's configuration. Also roll apps' services (docs/apps/README.md,
"Terraform"), machine clients, and roll search's clusters, API keys and snapshot repositories
(docs/search/README.md, "Terraform").

```hcl
provider "roll" {
  # Or ROLL_API_URL, ROLL_TOKEN and ROLL_PROJECT, as the CLI reads them.
  api_url = "https://api.roll.example"
  project = "prj_…"
}

resource "roll_secrets_environment" "production" {
  slug      = "production"
  protected = true
  approvals = 1                 # left out: roll's (1 when protected) or the dashboard's
  approvers = ["role:owner"]    # left out: whoever the dashboard named
}

# Changes to /payments in staging (and every folder under it) need an approval.
resource "roll_secrets_folder_policy" "payments" {
  environment = "staging"
  path        = "/payments"
  approvals   = 1
}

resource "roll_secret" "stripe" {
  environment = roll_secrets_environment.production.slug
  key         = "STRIPE_KEY"
  value       = var.stripe_key
}

resource "roll_secret" "db" {
  environment = "staging"
  key         = "DATABASE_URL"
  link = { kind = "postgres", ref = "ep_…", field = "url", mode = "dynamic" }
}

# CI logs in with its own OIDC token: no stored token (needs the operator's or a person's token).
resource "roll_secrets_identity" "ci" {
  name         = "ci"
  issuer       = "https://token.actions.githubusercontent.com"
  audience     = "https://api.roll.example"
  subject      = "repo:acme/web:ref:refs/heads/main"
  environments = ["staging"]
  actions      = ["read", "reveal"]
}

# A token for a service: a roll token with one rule on the project (revoked on destroy).
resource "roll_secrets_token" "web" {
  name         = "web"
  environments = ["production"]
  actions      = ["read", "reveal"]
  expires_at   = "2027-01-01T00:00:00Z"
}

resource "roll_secrets_sync" "vercel" {
  environment = "production"
  kind        = "vercel"
  target      = { project = "web", token_key = "VERCEL_TOKEN" }
}

data "roll_secrets_values" "production" {
  environment = "production"
}

# Without values in the state (Terraform 1.11+): read for this run only, write write-only.
ephemeral "roll_secrets_values" "production" {
  environment = "production"
  dynamic     = true # a dynamic credential for the run, revoked when it closes
}

resource "roll_secret" "staging_db" {
  environment      = "staging"
  key              = "DB_URL"
  value_wo         = ephemeral.roll_secrets_values.production.values["DB_URL"]
  value_wo_version = 1 # bump to write it again
}

# What a project registers itself: credentials go in write-only (Terraform 1.11+), from ephemeral
# variables or resources, so they reach neither the plan nor the state.
resource "roll_secrets_rotator" "vendor" {
  name          = "vendor"
  url           = "https://rotate.example/roll"   # .secret: its signing secret, shown once
  rotate_secret = { round = "1" }                 # change it to rotate the signing secret
  # A URL with a token in it goes in write-only instead of url:
  #   url_wo         = "https://rotate.example/roll?token=${var.hook_token}"
  #   url_wo_version = 1                            # bump it to send a new one (in place)
}

resource "roll_secrets_audit_sink" "splunk" {
  url              = "https://splunk.example/services/collector"   # or bucket:…, search:…, stream:…
  format           = "splunk"                                      # roll (signed), datadog, bearer
  token_wo         = var.hec_token
  token_wo_version = 1
}

resource "roll_secrets_github_app" "deploy" {
  name                   = "deploy"
  app_id                 = 123
  installation_id        = 456
  private_key_wo         = var.github_app_key
  private_key_wo_version = 1
  repositories           = ["web"]
  permissions            = { contents = "write", pull_requests = "write" }
}

resource "roll_secrets_aws_role" "deploy" {
  name     = "deploy"
  role_arn = "arn:aws:iam::123456789012:role/roll"
  trust    = "oidc"                  # or external_id (the default)
}

resource "roll_secrets_aws_user" "legacy" {
  name = "legacy"
  role = roll_secrets_aws_role.deploy.id   # an external_id role
  user = "legacy-ci"
}

# A vendor's own keys, issued and revoked by roll with a credential it keeps sealed.
resource "roll_secrets_saas_rotator" "gitlab" {
  name                  = "gitlab"
  vendor                = "gitlab"
  target                = jsonencode({ project = "acme/web", scopes = ["read_api"], expires_days = 90 })
  credential_wo         = var.gitlab_admin_token   # an ephemeral variable: never in the state
  credential_wo_version = 1                        # bump it to replace the credential in place
  credential_expires_at = "2027-03-31T00:00:00Z"   # health warns two weeks before
}

resource "roll_secrets_cloud_account" "gcp" {
  name            = "gcp"
  cloud           = "gcp"            # or azure: tenant, client_id, scope
  audience        = "//iam.googleapis.com/projects/1/locations/global/workloadIdentityPools/roll/providers/roll"
  service_account = "deploy@acme.iam.gserviceaccount.com"
}

# The project's whole list: one per project.
resource "roll_secrets_required_keys" "all" {
  keys = [
    { key = "DATABASE_URL" },
    { key = "PORT", path = "/api", type = "port", owner = "@payments" },
  ]
}

resource "roll_secrets_kms_key" "cmk" {
  key = "arn:aws:kms:eu-west-1:123456789012:key/…"
}

# roll apps: a service, a variable, a custom domain, an environment, a link and a notification
# target (docs/apps/http-api.md).
resource "roll_app_service" "web" {
  name          = "web"
  git_url       = "https://github.com/acme/web"   # or image, or repo (roll code); none: roll up
  # git_username = "deploy"; git_token = var.gitlab_token   # a private repository's login
  max_instances = 3
}

resource "roll_app_variable" "level" {
  service = roll_app_service.web.id
  name    = "LOG_LEVEL"
  value   = "info"
}

resource "roll_app_domain" "www" {
  service  = roll_app_service.web.id
  hostname = "www.example.com"   # point its CNAME at .target
}

resource "roll_app_environment" "staging" {
  service = roll_app_service.web.id
  name    = "staging"
  branch  = "develop"            # none: it deploys only when asked
}

resource "roll_app_link" "orders" {
  service = roll_app_service.web.id
  kind    = "postgres"           # redis, bucket, stream, nosql, search, service
  ref     = "orders"
  prefix  = "ORDERS"             # ORDERS_DATABASE_URL; .vars names them
}

resource "roll_app_notification" "failures" {
  kind   = "slack"
  url    = var.slack_webhook
  events = ["deploy.failed", "crash_loop"]
}

data "roll_app_service" "api" {
  name = "api"                   # made elsewhere: .url, .environments
}

# A machine client (docs/permissions.md, "Machine clients"): GitHub Actions on acme/web's main
# branch trades the job's own OIDC token at .token_endpoint (client id .client_id) for an hour's
# token that may deploy apps. Needs the operator's token (ROLL_TOKEN), not a scoped one.
resource "roll_machine_client" "web_ci" {
  name = "web-ci"

  rule {
    product   = "apps"
    resources = "*"                # or one resource block per resource, below
    actions   = { read = "allow", deploy = "allow" }
  }
  rule {
    product = "secrets"
    resource { project = "prj_…" } # or resource { id = "…" index = "…" }
    environments = ["production"]
    actions      = { read = "allow", reveal = "ask" }   # ask: a person approves each use
  }

  oidc {
    issuer  = "https://token.actions.githubusercontent.com"
    subject = "repo:acme/web:ref:refs/heads/main"
    claims  = { repository_owner_id = "1234" }
    # audience: roll's API URL when left out
  }
  # or, instead of oidc, a key it signs with: public_jwk = jsonencode({ kty = "OKP", crv = "Ed25519", x = "…" })
}

# roll search (docs/search/README.md, "Terraform"): a cluster, a key for a client, a snapshot
# repository. The operator's token, an admin's, or a person's own (their clusters, within their caps).
resource "roll_search_cluster" "shop" {
  name      = "shop"
  flavor    = "elasticsearch"    # or opensearch; switched in place, data kept
  always_on = false              # or idle_timeout_s = 900; 0 is always on
  ip_allowlist = [
    { cidr = "203.0.113.0/24", description = "office" },
  ]
  lifecycle { prevent_destroy = true }   # a destroy is restorable for 7 days, but still a delete
}

resource "roll_search_api_key" "web" {
  cluster          = roll_search_cluster.shop.id
  name             = "web"
  role_descriptors = jsonencode({ reader = { indices = [{ names = ["products"], privileges = ["read"] }] } })
  expiration       = "90d"
  # or access = "data" | "admin" instead of role_descriptors (OpenSearch: .username and .password)

  # Keys don't carry over a flavor switch: make it again in the same apply.
  lifecycle { replace_triggered_by = [roll_search_cluster.shop.flavor] }
}

resource "roll_search_snapshot_repository" "backups" {
  cluster     = roll_search_cluster.shop.id
  name        = "backups"
  type        = "s3"                                    # gcs, azure
  settings    = { bucket = "acme-search", base_path = "shop" }
  credentials = { access_key = var.aws_key_id, secret_key = var.aws_secret }
}

data "roll_search_cluster" "logs" {
  name = "logs"                  # or id = "srch_…": .endpoint, .flavor, .version
}

# A key for this run only (Terraform 1.10+), in neither the plan nor the state, revoked when the
# run ends: for another provider that manages what's inside the cluster, or a provisioner.
ephemeral "roll_search_api_key" "deploy" {
  cluster    = roll_search_cluster.shop.id
  name       = "terraform"
  access     = "admin"
  expiration = "1h"
}

provider "elasticstack" {
  elasticsearch {
    endpoints = [roll_search_cluster.shop.endpoint]
    api_key   = ephemeral.roll_search_api_key.deploy.encoded
  }
}
```

- A machine client has exactly one of `oidc` and `public_jwk`: roll's third way, a secret it
  makes, isn't offered, so no secret lands in the state. A `rule` names its resources as
  `resources = "*"` or as `resource` blocks (`id`, optionally `index`, or `project`), never both;
  its other fields are roll_auth's scope names. Rules, `expires_at`, `ip_allow` and
  `rate_per_minute` change in place on the client's grant. A changed `oidc` or `public_jwk`
  rotates it, keeping its client id (`keep_old_for_s` keeps the old credential working that long).
  A new `name` replaces it. It isn't in a project, so the provider's `project` doesn't apply.
  Import it by its client id (`mcl_…`): `oidc` is read back except its `jwks` (roll only counts
  the pinned keys), and a `public_jwk` isn't read back at all, so when either is in the file the
  first apply after an import registers it again (a rotate).

- A search cluster's `name` replaces it; everything else changes in place (`PATCH`), `flavor`
  too: roll switches the cluster between Elasticsearch and OpenSearch keeping its id and its data
  (it reloads as soon as no request runs), where a replacement would delete it and make a new,
  empty one. What a switch doesn't keep: links made before it are reissued by roll (roll secrets'
  and roll apps' search links), and keys don't carry over, since an Elasticsearch API key isn't
  an OpenSearch user. A `roll_search_api_key` remembers the flavor it was made on and is gone
  from the state once the cluster's differs, so the next apply makes it again; give it
  `lifecycle { replace_triggered_by = [roll_search_cluster.<name>.flavor] }` to make it again in
  the same apply as the switch.
  `always_on` and `idle_timeout_s` are one setting: either decides the other, and turning
  `always_on` off without a timeout goes back to the node's default. Settings left out keep what
  roll has, except `ip_allowlist`, which Terraform owns: left out, it's empty (anyone). Entries
  stay as written (`10.1.2.3/8`) while roll keeps them canonical (`10.0.0.0/8`). There's no
  version argument: roll answers as one Elasticsearch and one OpenSearch version per node (the
  computed `version`). Destroying a cluster deletes it; `roll search clusters restore` brings it
  back within the node's retention window (7 days by default). There's no `deletion_protection`:
  roll has no such setting to enforce it, so use `lifecycle { prevent_destroy = true }`. A
  create past a person's caps (50 clusters, 2 always-on by default), or a change that makes one
  always-on past them, fails with the cap, the count and the operator's flag that raises it.
  `password` is the cluster's own, from when it's made (not after an import); give clients a key.
  Import a cluster by its id or its name.
- A search API key never changes in place: every argument replaces it, and destroying it revokes
  it. Without `role_descriptors` it is made through roll's keys route (`access`, `data` by
  default, as `roll search keys create`); with them through the cluster's own
  `_security/api_key` (Elasticsearch only), encoded as roll encodes its keys
  (`base64(<id>@<cluster>:<key>)`). `encoded` (OpenSearch: `password`) is sensitive and in the
  state; the ephemeral `roll_search_api_key` keeps it out (revoked when the run closes). It
  isn't importable: its secret shows only once.
- Reading a search key or repository never wakes a sleeping cluster: a key is gone with its
  cluster or past its expiry, and on a loaded cluster a key revoked elsewhere, or a repository
  changed or removed elsewhere, shows in the plan. A repository's `credentials` are never read
  back (roll seals them apart from its settings), and only the `settings` in the configuration
  are compared. Import a repository as `srch_…/<name>`; the first apply registers it again with
  the configuration's credentials. Keys made with `role_descriptors`, repositories and the key
  check use the cluster's console route (`admin` for a scoped token); the keys route is
  `reveal`, creates `write`, changes `admin`, destroys `delete`.
- TODO (roll search): remote-cluster links (#347's `/v1/search/clusters/{id}/remotes/{alias}`) and
  allowed origins (browser search), once each is on main.

- An app's variable isn't read back (a sealed one never shows again): a change made outside
  Terraform is put back only when the value in the file changes. Its value lands in the state.

- Identities, syncs and tokens don't change in place: every attribute replaces them, and so do
  `roll_secrets_ca`, `roll_secrets_ssh_ca`, `roll_secrets_transit_key` and
  `roll_secrets_outside_database` (whose `admin_url_wo` is write-only: bump
  `admin_url_wo_version` to register it again).

- Rotators, audit sinks, GitHub Apps, AWS roles and users and cloud accounts import by
  `prj_…/<id>` (or the id alone, in the provider's project); the required keys and the KMS key by
  `prj_…`. A write-only credential (`token_wo`, `private_key_wo`) and a once-shown `secret` aren't
  imported: the first apply after an import with a `…_wo_version` in the file registers it again
  (a replace), and a rotator's `rotate_secret` there rotates its secret into the state. A rotator's
  URL isn't read back either (only its host): the first apply sends it again (`url`, or `url_wo`
  with its `url_wo_version`), in place.
- A rotator's name and URL, an AWS role's name, ARN and trust, and an AWS user's name and role
  change in place; their other attributes, and every attribute of a sink, an App or a cloud
  account, replace them. A rotator's `url` is in the plan and the state (sensitive), so a URL that
  carries a token goes in `url_wo` instead (write-only, Terraform 1.11+; bump `url_wo_version` to
  send a new one, in place).
- `roll_secrets_required_keys` owns the project's whole list: a key left out isn't required, and a
  second one on the same project undoes the first. Write folders without a trailing `/` (roll keeps
  them so, and an import reads them so). Destroying it requires nothing.
- `roll_secrets_kms_key` checks roll can use the key when it's set; destroying it unwraps the data
  key, so KMS must still answer.

- A notification target's URL is never read back, and a new URL is a new target. A webhook's
  signing secret is in `.secret` from when it's made. Filters left out are the defaults.

- A key's value is read back on refresh, references as written, so a change made outside
  Terraform shows in the plan (the token needs `reveal`).
- In a protected environment a write opens a change request for someone else to approve, and
  Terraform reports it as an error.
- Values in `roll_secret`'s `value` and the data source land in Terraform's state: keep the state as
  private as the values, or use `ephemeral "roll_secrets_values"` and `value_wo`, which never do.

Tests: `TF_ACC=1 go test ./...` with `terraform` on `PATH` (CI's `terraform` job).
