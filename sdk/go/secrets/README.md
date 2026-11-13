# roll secrets for Go

A client for roll secrets (docs/secrets.md, "Consumption"), with the same surface as
`@roll/secrets`: an environment's values, the runtime resolve (sealed to a key only this
process holds, if asked), the digest that moves when the values would, and a watch that
follows it and renews dynamic leases. Standard library only.

```go
// ROLL_API_URL and ROLL_TOKEN (a roll token with a rule on the project), or ROLL_IDENTITY
// for a workload identity's login with the workload's own OIDC token (GitHub Actions,
// ROLL_OIDC_TOKEN or ROLL_OIDC_TOKEN_FILE): no long-lived token in CI.
c, err := secrets.FromEnv(ctx)

v, err := c.Values(ctx, "production", "")          // v.Values["DATABASE_URL"]

// What a running service gets, sealed to a key only this process holds.
key, _ := secrets.GenerateKey()
v, err = c.ResolveSealed(ctx, "production", "", "app:web", key)

// Reload when the environment changes; dynamic credentials' leases are renewed meanwhile.
err = c.Watch(ctx, "production", secrets.Watch{OnChange: func(v *secrets.Values) error {
	return reload(v.Values)
}})
```

Tests: `go test -race ./...` here (CI's `sdk` job), or without Go installed:
`docker run --rm -v "$PWD":/src -w /src golang:1.24-bookworm go test -race ./...`.
