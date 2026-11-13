// Package secrets is a client for roll secrets (docs/secrets.md, "Consumption"): an environment's
// values, the runtime resolve (sealed to a key only this process holds, if asked), the digest that
// changes when the values would, and a watch that follows it. It mirrors @roll/secrets.
//
// A client holds a roll token (`rlt_…`, roll-access's) with a rule on one project, the operator
// token, or the short-lived token a workload identity's login gives (Login, FromEnv).
package secrets

import (
	"bytes"
	"context"
	"crypto/ecdh"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"strings"
	"sync"
	"time"
)

// Client reads one project's secrets.
type Client struct {
	// API is rolld's API, e.g. https://api.roll.example (no trailing /v1).
	API string
	// Token is a roll token with a rule on the project, or the operator token.
	Token string
	// Project is the project's id (prj_…). A token with a rule on one project knows it, so it may
	// be left empty.
	Project string
	// HTTP is the client requests go through; http.DefaultClient when nil.
	HTTP *http.Client

	once    sync.Mutex
	project string
}

// Problem is a key that didn't resolve (a reference to nothing, a cycle, a link that failed).
type Problem struct {
	Key     string `json:"key"`
	Problem string `json:"problem"`
}

// Values are an environment's values. Dynamic keys hold a short-lived credential that ends with
// its lease (Leases; Renew keeps it); Linked keys come from a link to a resource, whose address a
// container network may need rewritten.
type Values struct {
	Values   map[string]string `json:"values"`
	Problems []Problem         `json:"problems"`
	Leases   []string          `json:"leases"`
	Dynamic  []string          `json:"dynamic"`
	Linked   []string          `json:"linked"`
}

// Lease is a dynamic credential's lease.
type Lease struct {
	ID        string `json:"id"`
	Env       string `json:"environment"`
	Path      string `json:"path"`
	Key       string `json:"key"`
	Kind      string `json:"kind"`
	Consumer  string `json:"consumer"`
	IssuedAt  int64  `json:"issued_at"`
	ExpiresAt int64  `json:"expires_at"`
	RevokedAt *int64 `json:"revoked_at"`
}

// Error is an answer that wasn't a success: its status, roll's code (`forbidden`, `unresolved`…)
// and message. It never holds a value.
type Error struct {
	Status  int
	Code    string
	Message string
}

func (e *Error) Error() string { return e.Message }

// New is a client for api with token.
func New(api, token string) *Client {
	return &Client{API: api, Token: token}
}

func (c *Client) call(ctx context.Context, method, path string, body, out any) error {
	var r io.Reader
	if body != nil {
		b, err := json.Marshal(body)
		if err != nil {
			return err
		}
		r = bytes.NewReader(b)
	}
	req, err := http.NewRequestWithContext(ctx, method, strings.TrimRight(c.API, "/")+path, r)
	if err != nil {
		return err
	}
	if c.Token != "" {
		req.Header.Set("Authorization", "Bearer "+c.Token)
	}
	if body != nil {
		req.Header.Set("Content-Type", "application/json")
	}
	h := c.HTTP
	if h == nil {
		h = http.DefaultClient
	}
	res, err := h.Do(req)
	if err != nil {
		return err
	}
	defer res.Body.Close()
	data, err := io.ReadAll(io.LimitReader(res.Body, 16<<20))
	if err != nil {
		return err
	}
	if res.StatusCode >= 300 {
		var e struct {
			Error string `json:"error"`
			Code  string `json:"code"`
		}
		_ = json.Unmarshal(data, &e)
		if e.Error == "" {
			e.Error = res.Status
		}
		return &Error{Status: res.StatusCode, Code: e.Code, Message: e.Error}
	}
	if out == nil || len(data) == 0 {
		return nil
	}
	return json.Unmarshal(data, out)
}

// ProjectID is the project: as given, or the token's own (GET /v1/secrets/whoami).
func (c *Client) ProjectID(ctx context.Context) (string, error) {
	if c.Project != "" {
		return c.Project, nil
	}
	c.once.Lock()
	defer c.once.Unlock()
	if c.project != "" {
		return c.project, nil
	}
	var w struct {
		Project *string `json:"project"`
	}
	if err := c.call(ctx, http.MethodGet, "/v1/secrets/whoami", nil, &w); err != nil {
		return "", err
	}
	if w.Project == nil || *w.Project == "" {
		return "", &Error{Status: 400, Code: "project", Message: "this token isn't limited to one project: set Project"}
	}
	c.project = *w.Project
	return c.project, nil
}

func (c *Client) env(ctx context.Context, env string) (string, error) {
	p, err := c.ProjectID(ctx)
	if err != nil {
		return "", err
	}
	return "/v1/secrets/projects/" + url.PathEscape(p) + "/environments/" + url.PathEscape(env), nil
}

func query(path string) string {
	if path == "" {
		return ""
	}
	return "?path=" + url.QueryEscape(path)
}

func orRoot(path string) string {
	if path == "" {
		return "/"
	}
	return path
}

// Values are env's values under path ("" for the root), references resolved.
func (c *Client) Values(ctx context.Context, env, path string) (*Values, error) {
	e, err := c.env(ctx, env)
	if err != nil {
		return nil, err
	}
	var v Values
	return &v, c.call(ctx, http.MethodGet, e+"/values"+query(path), nil, &v)
}

// Resolve is env's values as consumer (`app:web`), what a running service gets: dynamic
// credentials' leases are the consumer's, and the read is the consumer's in the audit log.
func (c *Client) Resolve(ctx context.Context, env, path, consumer string) (*Values, error) {
	e, err := c.env(ctx, env)
	if err != nil {
		return nil, err
	}
	var v Values
	return &v, c.call(ctx, http.MethodPost, e+"/resolve", map[string]any{"consumer": consumer, "path": orRoot(path)}, &v)
}

// ResolveSealed is Resolve with the values sealed to key on the server and opened here: they are
// never in the clear between the two.
func (c *Client) ResolveSealed(ctx context.Context, env, path, consumer string, key *ecdh.PrivateKey) (*Values, error) {
	e, err := c.env(ctx, env)
	if err != nil {
		return nil, err
	}
	var s struct {
		Sealed   Sealed    `json:"sealed"`
		Context  string    `json:"context"`
		Problems []Problem `json:"problems"`
		Leases   []string  `json:"leases"`
		Dynamic  []string  `json:"dynamic"`
		Linked   []string  `json:"linked"`
	}
	if err := c.call(ctx, http.MethodPost, e+"/resolve", map[string]any{"consumer": consumer, "path": orRoot(path), "seal_to": PublicKey(key)}, &s); err != nil {
		return nil, err
	}
	plain, err := Open(s.Sealed, key, s.Context)
	if err != nil {
		return nil, err
	}
	v := Values{Problems: s.Problems, Leases: s.Leases, Dynamic: s.Dynamic, Linked: s.Linked}
	if err := json.Unmarshal(plain, &v.Values); err != nil {
		return nil, errors.New("the sealed values aren't a map")
	}
	return &v, nil
}

// Digest moves whenever env's values under path would; it reads no value.
func (c *Client) Digest(ctx context.Context, env, path string) (string, error) {
	e, err := c.env(ctx, env)
	if err != nil {
		return "", err
	}
	var d struct {
		Digest string `json:"digest"`
	}
	return d.Digest, c.call(ctx, http.MethodGet, e+"/digest"+query(path), nil, &d)
}

// Renew pushes a dynamic lease's expiry out by ttlMinutes (0: its link's TTL).
func (c *Client) Renew(ctx context.Context, lease string, ttlMinutes int) (*Lease, error) {
	p, err := c.ProjectID(ctx)
	if err != nil {
		return nil, err
	}
	body := map[string]any{}
	if ttlMinutes > 0 {
		body["ttl_minutes"] = ttlMinutes
	}
	var l Lease
	return &l, c.call(ctx, http.MethodPost, "/v1/secrets/projects/"+url.PathEscape(p)+"/leases/"+url.PathEscape(lease)+"/renew", body, &l)
}

// Watch is what Client.Watch follows and how often.
type Watch struct {
	Path string
	// Consumer, when set, resolves as it (Resolve) instead of reading (Values).
	Consumer string
	// Every is how often the digest is checked (default 5 s).
	Every time.Duration
	// RenewEvery is how often the latest values' dynamic leases are renewed (default 10 minutes).
	RenewEvery time.Duration
	// OnChange gets the values at the start and again each time they change. Values that don't
	// resolve never reach it: what's running keeps the previous ones.
	OnChange func(*Values) error
	// OnError gets a check, a fetch or a renewal that failed; the watch goes on.
	OnError func(error)
}

// Watch calls w.OnChange with env's values now, then again whenever its digest moves, renewing
// the dynamic leases of the latest values meanwhile (as `roll run --watch` does), until ctx ends.
func (c *Client) Watch(ctx context.Context, env string, w Watch) error {
	every, renewEvery := w.Every, w.RenewEvery
	if every <= 0 {
		every = 5 * time.Second
	}
	if renewEvery <= 0 {
		renewEvery = 10 * time.Minute
	}
	fail := w.OnError
	if fail == nil {
		fail = func(error) {}
	}
	var last string
	var leases []string
	tick := func() {
		now, err := c.Digest(ctx, env, w.Path)
		if err != nil {
			fail(err)
			return
		}
		if now == last {
			return
		}
		var v *Values
		if w.Consumer != "" {
			v, err = c.Resolve(ctx, env, w.Path, w.Consumer)
		} else {
			v, err = c.Values(ctx, env, w.Path)
		}
		if err != nil {
			fail(err) // the next tick tries again
			return
		}
		before := last
		last = now
		if len(v.Problems) > 0 {
			// Nothing running yet: try again each tick. Otherwise the next change tries again.
			if before == "" {
				last = ""
			}
			fail(&Error{Status: 409, Code: "unresolved", Message: fmt.Sprintf("%s doesn't resolve: %s: %s", env, v.Problems[0].Key, v.Problems[0].Problem)})
			return
		}
		leases = v.Leases
		if err := w.OnChange(v); err != nil {
			fail(err)
		}
	}
	tick()
	check, renew := time.NewTicker(every), time.NewTicker(renewEvery)
	defer check.Stop()
	defer renew.Stop()
	for {
		select {
		case <-ctx.Done():
			return ctx.Err()
		case <-check.C:
			tick()
		case <-renew.C:
			for _, l := range leases {
				if _, err := c.Renew(ctx, l, 0); err != nil {
					fail(err)
				}
			}
		}
	}
}
