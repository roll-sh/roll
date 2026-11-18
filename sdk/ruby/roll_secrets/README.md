# roll secrets for Ruby

A client for roll secrets (docs/secrets.md, "Consumption") with the same surface as
`@roll/secrets`, `sdk/go/secrets` and `sdk/python/roll_secrets`: an environment's values, the
runtime resolve (sealed to a key only this process holds, if asked), the digest that moves when the
values would, a watch that follows it and renews dynamic leases, and transit encryption keys.
Ruby's standard library only (`net/http`, `json`, `openssl`), sealed resolves included.

```ruby
require "roll_secrets"

# ROLL_API_URL and ROLL_TOKEN (a roll token with a rule on the project), or ROLL_IDENTITY for a
# workload identity's login with the workload's own OIDC token: no long-lived token in CI.
c = RollSecrets.from_env
v = c.values("production")                        # v.values["DATABASE_URL"]

key = RollSecrets.generate_key                     # sealed to a key only this process holds
v = c.resolve_sealed("production", "app:web", key)

stop = RollSecrets::Stop.new                       # stop.set ends the watch
Thread.new { c.watch("production", stop: stop) { |v| reload(v.values) } }

cards = c.transit("cards")                         # an encryption key that stays in roll
ct = cards.encrypt(card_number, "order:42")
# Read per request? Keep answers 30 s, then revalidate (a 304 while unchanged).
c = RollSecrets::Client.new(api, token, cache_ttl: 30)
```

Failures raise `RollSecrets::SecretsError` (`status`, `code` and roll's message, never a value).
`inspect` on `Values`, `Client` and `Transit` names keys and ids, never a value or a token.

## Rails

Load an environment's values into `ENV` at boot, before anything reads them, in
`config/application.rb` after `Bundler.require`:

```ruby
# config/application.rb
require "roll_secrets"

# What's already set wins, so a developer's own .env or shell still overrides roll.
RollSecrets.from_env.resolve(ENV.fetch("ROLL_ENVIRONMENT", Rails.env), "app:web").values.each do |k, v|
  ENV[k] ||= v
end
```

`database.yml`, `cable.yml` and initializers then read `ENV["DATABASE_URL"]` as usual. To keep
values out of `ENV` (child processes inherit it), hold them in the configuration instead, the way
`Rails.application.credentials` is read:

```ruby
# config/initializers/roll_secrets.rb
Rails.application.config.x.secrets =
  RollSecrets.from_env.resolve(ENV.fetch("ROLL_ENVIRONMENT", Rails.env), "app:web").values.freeze

# anywhere
Stripe.api_key = Rails.configuration.x.secrets.fetch("STRIPE_SECRET_KEY")
```

`resolve` with a consumer (`app:web`) is what a running service gets: dynamic credentials' leases
are the consumer's, and the read is the consumer's in the audit log. Use `values` for a one-off
read. A server with several processes (Puma's workers) reads once at boot, before forking.

Tests: `rake` here (or `ruby -Ilib -Itest -e 'Dir["test/*_test.rb"].each { |f| require_relative f }'`;
CI's `sdk` job), or without Ruby installed:
`docker run --rm -v "$PWD":/src -w /src ruby:3.3-slim rake`.
