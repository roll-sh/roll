# roll secrets for Go

A client for roll secrets (docs/secrets.md, "Consumption"), with the same surface as
`@roll/secrets`: an environment's values, the runtime resolve (sealed to a key only this
process holds, if asked), the digest that moves when the values would, a watch that
follows it and renews dynamic leases, one key's record and stored value, and one key's
writes. Standard library only.

```go
// ROLL_API_URL and ROLL_TOKEN (a roll token with a rule on the project), or ROLL_IDENTITY
// for a workload identity's login with the workload's own OIDC token (GitHub Actions,
// ROLL_OIDC_TOKEN or ROLL_OIDC_TOKEN_FILE) or, on AWS, its own IAM role (a signed
// GetCallerIdentity; docs/secrets.md, "AWS IAM"): no long-lived token in CI.
c, err := secrets.FromEnv(ctx)

v, err := c.Values(ctx, "production", "")          // v.Values["DATABASE_URL"]

// What a running service gets, sealed to a key only this process holds.
key, _ := secrets.GenerateKey()
v, err = c.ResolveSealed(ctx, "production", "", "app:web", key)

// Reload when the environment changes; dynamic credentials' leases are renewed meanwhile.
// A TOTP key's one-time code (docs/secrets.md, "TOTP keys"), or one from a seed already resolved.
otp, err := c.TOTP(ctx, "production", "", "REGISTRY_TOTP")   // otp.Code, otp.ExpiresAt
otp, err = secrets.TOTPAt(v.Values["REGISTRY_TOTP"], time.Now())

// An encryption key (docs/secrets.md, "Encryption keys"): the key stays in roll.
cards := c.Transit("cards")
ct, err := cards.Encrypt(ctx, []byte(cardNumber), "order:42")
// Read per request? Keep answers 30 s, then revalidate (a 304 while unchanged), at most
// 100 folders.
c.CacheTTL, c.CacheSize = 30*time.Second, 100

// An OIDC token that changes (a rotated file, one a cloud mints): asked for at each login.
c, err = secrets.LoginFunc(ctx, api, "idn_…", func(ctx context.Context) (string, error) { return mint(ctx) })

err = c.Watch(ctx, "production", secrets.Watch{OnChange: func(v *secrets.Values) error {
	return reload(v.Values)
}})

// Nothing here renews a lease? Issue no dynamic credential: those keys are left out and
// named in v.Dynamic, and the answer can be kept by CacheTTL.
v, err = c.Values(ctx, "production", "", secrets.WithoutDynamic())

// One key's writes. Set replaces a link the key had; Delete of a key that isn't there is
// Deleted false. A protected environment, or a folder that needs approval, is an *Error with
// Code "approval_required" (open a change request instead). Errors never hold the value, and
// both drop what CacheTTL kept.
w, err := c.Set(ctx, "staging", "/api", "FEATURE_FLAGS", "beta", secrets.SetOptions{})
d, err := c.Delete(ctx, "staging", "/api", "OLD_KEY") // d.Deleted

// One key's reads, for a tool that writes only what differs. Secret is the key's record
// without its value (Version, Comment, LinkKind, Source.Kind "own", "base" or "import"), nil
// when the folder has no such key. StoredValue is the value as stored, references unresolved:
// a reveal, audited and never kept by CacheTTL, and no error holds it.
s, err := c.Secret(ctx, "staging", "/api", "FEATURE_FLAGS")
stored, err := c.StoredValue(ctx, "staging", "/api", "FEATURE_FLAGS")

// A workload identity's login with a token source called for a fresh OIDC token at each
// login (FromEnv uses secrets.OIDCTokenFromEnv).
c, err = secrets.LoginWithIdentity(ctx, api, "idn_…", func(ctx context.Context) (string, error) {
	b, err := os.ReadFile(tokenFile)
	return string(b), err
})
```

Tests: `go test -race ./...` here (CI's `sdk` job), or without Go installed:
`docker run --rm -v "$PWD":/src -w /src golang:1.24-bookworm go test -race ./...`.
