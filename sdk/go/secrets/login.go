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
)

// Login exchanges a workload's own OIDC token for a short-lived roll token through workload
// identity (docs/secrets.md, "Workload identities"): nothing long-lived sits in the workload's
// settings. The answer's client is limited to the identity's project, environments and folders.
func Login(ctx context.Context, api, identity, oidcToken string) (*Client, error) {
	c := &Client{API: api}
	var l struct {
		Token   string `json:"token"`
		Project string `json:"project"`
	}
	if err := c.call(ctx, http.MethodPost, "/v1/secrets/login", map[string]string{"identity": identity, "token": strings.TrimSpace(oidcToken)}, &l); err != nil {
		return nil, err
	}
	if l.Token == "" {
		return nil, errors.New("the login answered no token")
	}
	return &Client{API: api, Token: l.Token, Project: l.Project}, nil
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
		oidc, err := oidcToken(ctx, api, env)
		if err != nil {
			return nil, err
		}
		if c, err = Login(ctx, api, env("ROLL_IDENTITY"), oidc); err != nil {
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
