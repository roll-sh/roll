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

data "roll_secrets_values" "production" {
  environment = "production"
}
```

- A key's value is read back on refresh, references as written, so a change made outside
  Terraform shows in the plan (the token needs `reveal`).
- In a protected environment a write opens a change request for someone else to approve, and
  Terraform reports it as an error.
- Values in `roll_secret` and the data source land in Terraform's state: keep the state as
  private as the values.

Tests: `TF_ACC=1 go test ./...` with `terraform` on `PATH` (CI's `terraform` job).
