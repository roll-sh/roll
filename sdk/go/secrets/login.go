package secrets

import (
	"context"
	"encoding/json"
	"errors"
	"io"
	"net/http"
	"net/url"
	"os"
	"strings"
	"sync"
	"time"
)

// Login exchanges a workload's own OIDC token for a short-lived roll token through workload
// identity (docs/secrets.md, "Workload identities"): nothing long-lived sits in the workload's
// settings. The answer's client is limited to the identity's project, environments and folders,
// and logs in again with the same OIDC token before its roll token ends (FromEnv reads a fresh
// OIDC token each time).
func Login(ctx context.Context, api, identity, oidcToken string) (*Client, error) {
	return login(ctx, api, identity, func(context.Context) (string, error) { return oidcToken, nil })
}

func login(ctx context.Context, api, identity string, source func(context.Context) (string, error)) (*Client, error) {
	c := &Client{API: api, login: &relogin{identity: identity, source: source}}
	if err := c.login.renew(ctx, c); err != nil {
		return nil, err
	}
	return c, nil
}

// relogin keeps a logged-in client's token fresh: a new login once less than a fifth of its life
// (at least a minute) is left, or when the server refuses it.
type relogin struct {
	identity string
	source   func(context.Context) (string, error)
	mu       sync.Mutex
	issued   time.Time
	expires  time.Time
}

// token is the client's token, renewed first when it's about to end (or `stale`: refused).
func (l *relogin) token(ctx context.Context, c *Client, stale string) (string, error) {
	l.mu.Lock()
	defer l.mu.Unlock()
	margin := l.expires.Sub(l.issued) / 5
	if margin < time.Minute {
		margin = time.Minute
	}
	// An answer without an expiry is renewed only when the server refuses it.
	ending := !l.expires.IsZero() && time.Now().After(l.expires.Add(-margin))
	refused := stale != "" && stale == c.Token
	if refused || ending {
		// Renewing early that fails keeps the token it has until it really ends (the identity
		// provider or roll may be down for a moment).
		if err := l.renewLocked(ctx, c); err != nil && (refused || !time.Now().Before(l.expires)) {
			return "", err
		}
	}
	return c.Token, nil
}

func (l *relogin) renew(ctx context.Context, c *Client) error {
	l.mu.Lock()
	defer l.mu.Unlock()
	return l.renewLocked(ctx, c)
}

func (l *relogin) renewLocked(ctx context.Context, c *Client) error {
	oidc, err := l.source(ctx)
	if err != nil {
		return err
	}
	var a struct {
		Token     string `json:"token"`
		Project   string `json:"project"`
		ExpiresAt int64  `json:"expires_at"`
	}
	anon := &Client{API: c.API, HTTP: c.HTTP}
	if err := anon.call(ctx, http.MethodPost, "/v1/secrets/login", map[string]string{"identity": l.identity, "token": strings.TrimSpace(oidc)}, &a); err != nil {
		return err
	}
	if a.Token == "" {
		return errors.New("the login answered no token")
	}
	c.Token, l.issued, l.expires = a.Token, time.Now(), time.Time{}
	if a.ExpiresAt > 0 {
		l.expires = time.UnixMilli(a.ExpiresAt)
	}
	if c.Project == "" {
		c.Project = a.Project
	}
	return nil
}

// FromEnv is a client as `roll run` makes one: ROLL_API_URL, then ROLL_TOKEN, or ROLL_IDENTITY
// with the workload's OIDC token from ROLL_OIDC_TOKEN, ROLL_OIDC_TOKEN_FILE (a Kubernetes projected
// token) or GitHub Actions (`permissions: id-token: write`; audience ROLL_OIDC_AUDIENCE, default the
// API's URL). ROLL_PROJECT names the project when the token doesn't.
func FromEnv(ctx context.Context) (*Client, error) {
	env := func(k string) string { return strings.TrimSpace(os.Getenv(k)) }
	api := env("ROLL_API_URL")
	if api == "" {
		return nil, errors.New("ROLL_API_URL isn't set")
	}
	var c *Client
	switch {
	case env("ROLL_TOKEN") != "":
		c = New(api, env("ROLL_TOKEN"))
	case env("ROLL_IDENTITY") != "":
		// The OIDC token is read again for each login (a projected token is replaced on disk).
		source := func(ctx context.Context) (string, error) { return oidcToken(ctx, api, env) }
		var err error
		if c, err = login(ctx, api, env("ROLL_IDENTITY"), source); err != nil {
			return nil, err
		}
	default:
		return nil, errors.New("set ROLL_TOKEN, or ROLL_IDENTITY for a workload identity")
	}
	if p := env("ROLL_PROJECT"); p != "" {
		c.Project = p
	}
	return c, nil
}

func oidcToken(ctx context.Context, api string, env func(string) string) (string, error) {
	if t := env("ROLL_OIDC_TOKEN"); t != "" {
		return t, nil
	}
	if f := env("ROLL_OIDC_TOKEN_FILE"); f != "" {
		b, err := os.ReadFile(f)
		return string(b), err
	}
	reqURL, bearer := env("ACTIONS_ID_TOKEN_REQUEST_URL"), env("ACTIONS_ID_TOKEN_REQUEST_TOKEN")
	if reqURL == "" || bearer == "" {
		return "", errors.New("ROLL_IDENTITY is set but there's no OIDC token: set ROLL_OIDC_TOKEN or ROLL_OIDC_TOKEN_FILE, or run in GitHub Actions with `permissions: id-token: write`")
	}
	// The server's own URL by default: a token minted for one roll server isn't good at another.
	audience := env("ROLL_OIDC_AUDIENCE")
	if audience == "" {
		audience = strings.TrimRight(api, "/")
	}
	sep := "?"
	if strings.Contains(reqURL, "?") {
		sep = "&"
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, reqURL+sep+"audience="+url.QueryEscape(audience), nil)
	if err != nil {
		return "", err
	}
	req.Header.Set("Authorization", "Bearer "+bearer)
	res, err := http.DefaultClient.Do(req)
	if err != nil {
		return "", err
	}
	defer res.Body.Close()
	if res.StatusCode != http.StatusOK {
		return "", errors.New("GitHub Actions didn't give its OIDC token (does the job have `permissions: id-token: write`?)")
	}
	var v struct {
		Value string `json:"value"`
	}
	if err := json.NewDecoder(io.LimitReader(res.Body, 64<<10)).Decode(&v); err != nil || v.Value == "" {
		return "", errors.New("GitHub Actions' OIDC answer has no value")
	}
	return v.Value, nil
}
