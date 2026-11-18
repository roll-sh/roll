# The roll Terraform provider

roll secrets as code (docs/secrets.md, "Terraform"): a project's environments, its keys (a value,
or a link to a roll resource on the project's canvas) and, as a data source, an environment's
values for another provider's configuration.

```hcl
provider "roll" {
  # Or ROLL_API_URL, ROLL_TOKEN and ROLL_PROJECT, as the CLI reads them.
  api_url = "https://api.roll.example"
  project = "prj_…"
}

resource "roll_secrets_environment" "production" {
  slug      = "production"
  protected = true
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
  URL isn't read back either (only its host): the first apply sends it again, in place.
- A rotator's name and URL, an AWS role's name, ARN and trust, and an AWS user's name and role
  change in place; their other attributes, and every attribute of a sink, an App or a cloud
  account, replace them. A rotator's URL is in the state (sensitive), so keep a token out of it or
  keep the state private.
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
