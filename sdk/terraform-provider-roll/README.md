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

# roll apps: a service, a variable, a custom domain (docs/apps/http-api.md).
resource "roll_app_service" "web" {
  name          = "web"
  git_url       = "https://github.com/acme/web"   # or image, or repo (roll code); none: roll up
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
```

- An app's variable isn't read back (a sealed one never shows again): a change made outside
  Terraform is put back only when the value in the file changes. Its value lands in the state.

- Identities, syncs and tokens don't change in place: every attribute replaces them.

- A key's value is read back on refresh, references as written, so a change made outside
  Terraform shows in the plan (the token needs `reveal`).
- In a protected environment a write opens a change request for someone else to approve, and
  Terraform reports it as an error.
- Values in `roll_secret` and the data source land in Terraform's state: keep the state as
  private as the values.

Tests: `TF_ACC=1 go test ./...` with `terraform` on `PATH` (CI's `terraform` job).
