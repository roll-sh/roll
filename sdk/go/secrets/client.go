// Package secrets is a client for roll secrets (docs/secrets.md, "Consumption"): an environment's
// values, the runtime resolve (sealed to a key only this process holds, if asked), the digest that
// changes when the values would, a watch that follows it, and one key's writes. It mirrors
// @roll/secrets.
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
	"sort"
	"strconv"
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
	// CacheTTL keeps Values answers this long and answers from memory meanwhile; after that it asks
	// again with the digest they were read against, and an unchanged folder costs a 304 that reads,
	// issues and audits nothing. A folder holding dynamic credentials is never kept (they end with
	// their lease). Zero: off.
	CacheTTL time.Duration
	// Now is the clock CacheTTL is measured with (for tests); time.Now when nil.
	Now func() time.Time

	once    sync.Mutex
	project string
	// login renews the token of a client a workload identity's login made.
	login *relogin

	cacheMu sync.Mutex
	cache   map[string]cached
}

// cached is a Values answer kept by CacheTTL, with the digest it was read against.
type cached struct {
	values *Values
	etag   string
	at     time.Time
}

// Problem is a key that didn't resolve (a reference to nothing, a cycle, a link that failed).
type Problem struct {
	Key     string `json:"key"`
	Problem string `json:"problem"`
}

// Values are an environment's values. Dynamic keys hold a short-lived credential that ends with
// its lease (Leases; Renew keeps it); Linked keys come from a link to a resource, whose address a
// container network may need rewritten. Followed keys link a resource a preview branched: they
// aren't in Values, and roll previews sets them from its branch.
type Values struct {
	Values   map[string]string `json:"values"`
	Problems []Problem         `json:"problems"`
	Leases   []string          `json:"leases"`
	Dynamic  []string          `json:"dynamic"`
	Linked   []string          `json:"linked"`
	Followed []Followed        `json:"followed,omitempty"`
}

// String names the keys only, never a value: a Values printed or logged with %v or %+v.
func (v Values) String() string {
	keys := make([]string, 0, len(v.Values))
	for k := range v.Values {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	return fmt.Sprintf("Values{keys: %v, problems: %d, leases: %d}", keys, len(v.Problems), len(v.Leases))
}

// GoString is String, for %#v.
func (v Values) GoString() string { return v.String() }

// String never shows the token: a Client printed or logged with %v or %+v.
func (c *Client) String() string {
	return fmt.Sprintf("Client{API: %q, Project: %q}", c.API, c.Project)
}

// GoString is String, for %#v.
func (c *Client) GoString() string { return c.String() }

// Followed is a key left out because it links a resource the environment's preview branched.
type Followed struct {
	Name   string `json:"name"`
	Kind   string `json:"kind"`
	Parent string `json:"parent"`
	Field  string `json:"field"`
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
	return c.authed(ctx, func(token string) error { return c.send(ctx, token, method, path, body, out) })
}

// authed runs do with the client's token. A logged-in client's is renewed first when it's ending,
// and when the server refuses it (revoked, ended early): one new login, then once more.
func (c *Client) authed(ctx context.Context, do func(token string) error) error {
	if c.login == nil {
		return do(c.Token)
	}
	token, err := c.login.token(ctx, c, "")
	if err != nil {
		return err
	}
	err = do(token)
	var e *Error
	if errors.As(err, &e) && e.Status == http.StatusUnauthorized {
		if token, err = c.login.token(ctx, c, token); err != nil {
			return err
		}
		return do(token)
	}
	return err
}

func (c *Client) send(ctx context.Context, token, method, path string, body, out any) error {
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
	if token != "" {
		req.Header.Set("Authorization", "Bearer "+token)
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

// ValuesOption changes what Values reads.
type ValuesOption func(*valuesOptions)

type valuesOptions struct{ noDynamic bool }

// WithoutDynamic reads without issuing dynamic credentials (`dynamic=false`), for a reader that
// can't renew a lease: those keys are left out and named in Values.Dynamic, and such an answer can
// be kept by CacheTTL.
func WithoutDynamic() ValuesOption { return func(o *valuesOptions) { o.noDynamic = true } }

// Values are env's values under path ("" for the root), references resolved.
func (c *Client) Values(ctx context.Context, env, path string, opts ...ValuesOption) (*Values, error) {
	var o valuesOptions
	for _, f := range opts {
		f(&o)
	}
	e, err := c.env(ctx, env)
	if err != nil {
		return nil, err
	}
	u := e + "/values" + query(path)
	if o.noDynamic {
		if path == "" {
			u += "?dynamic=false"
		} else {
			u += "&dynamic=false"
		}
	}
	if c.CacheTTL > 0 {
		v, err := c.cachedValues(ctx, u, o.noDynamic)
		if err != nil {
			return nil, err
		}
		return v.clone(), nil
	}
	var v Values
	return &v, c.call(ctx, http.MethodGet, u, nil, &v)
}

// Change is one change to an environment: its number in the project, who made it and when.
type Change struct {
	Seq         int64  `json:"seq"`
	Environment string `json:"environment"`
	Path        string `json:"path"`
	Message     string `json:"message"`
	By          string `json:"by"`
	At          int64  `json:"at"`
	Keys        int    `json:"keys"`
	Kind        string `json:"kind"`
}

// Secret is a key's record, never its value.
type Secret struct {
	ID        string   `json:"id"`
	Key       string   `json:"key"`
	Path      string   `json:"path"`
	Type      string   `json:"type"`
	Version   int64    `json:"version"`
	Comment   string   `json:"comment"`
	Tags      []string `json:"tags"`
	FP        string   `json:"fp"`
	UpdatedAt int64    `json:"updated_at"`
	UpdatedBy string   `json:"updated_by"`
	ExpiresAt *int64   `json:"expires_at"`
}

// Written is what a write answers: the change (nil when nothing changed) and the key now.
type Written struct {
	Change   *Change  `json:"change"`
	Secret   *Secret  `json:"secret"`
	Warnings []string `json:"warnings"`
}

// Deleted is what Delete answers: Deleted is false when the key wasn't there.
type Deleted struct {
	Deleted bool
	Change  *Change
}

// SetOptions are Set's optional fields.
type SetOptions struct {
	// Comment replaces the key's comment; nil keeps it.
	Comment *string
	// Message is the change's.
	Message string
}

func (c *Client) secretURL(ctx context.Context, env, path, key string) (string, error) {
	e, err := c.env(ctx, env)
	if err != nil {
		return "", err
	}
	return e + "/secrets/" + url.PathEscape(key) + query(path), nil
}

// dropCache forgets every kept Values answer: a write may change any of them (a reference or an
// import elsewhere may follow the key).
func (c *Client) dropCache() {
	c.cacheMu.Lock()
	c.cache = nil
	c.cacheMu.Unlock()
}

// Set sets key to value in path's folder ("" for the root), replacing a link the key had. The same
// value again writes nothing (Written.Change is nil). A protected environment, or a folder that
// needs approval, is an *Error with the Code `approval_required` (open a change request instead).
// Errors never hold the value. Drops the kept Values answers.
func (c *Client) Set(ctx context.Context, env, path, key, value string, o SetOptions) (*Written, error) {
	u, err := c.secretURL(ctx, env, path, key)
	if err != nil {
		return nil, err
	}
	body := map[string]any{"value": value, "link": nil}
	if o.Comment != nil {
		body["comment"] = *o.Comment
	}
	if o.Message != "" {
		body["message"] = o.Message
	}
	defer c.dropCache()
	var w Written
	if err := c.call(ctx, http.MethodPut, u, body, &w); err != nil {
		return nil, err
	}
	return &w, nil
}

// Delete deletes key from path's folder. A key that isn't there (or only shows through a base or
// an import) is Deleted false, not an error; `approval_required` as for Set. Drops the kept Values
// answers.
func (c *Client) Delete(ctx context.Context, env, path, key string) (*Deleted, error) {
	u, err := c.secretURL(ctx, env, path, key)
	if err != nil {
		return nil, err
	}
	defer c.dropCache()
	var w Written
	if err := c.call(ctx, http.MethodDelete, u, nil, &w); err != nil {
		var e *Error
		if errors.As(err, &e) && e.Status == http.StatusNotFound {
			return &Deleted{}, nil
		}
		return nil, err
	}
	return &Deleted{Deleted: true, Change: w.Change}, nil
}

// freshValues is Values without CacheTTL (a watch that saw the digest move reads anew); a kept
// answer for the folder goes.
func (c *Client) freshValues(ctx context.Context, env, path string) (*Values, error) {
	e, err := c.env(ctx, env)
	if err != nil {
		return nil, err
	}
	var v Values
	if err := c.call(ctx, http.MethodGet, e+"/values"+query(path), nil, &v); err != nil {
		return nil, err
	}
	c.cacheMu.Lock()
	delete(c.cache, e+"/values"+query(path))
	c.cacheMu.Unlock()
	return &v, nil
}

// clone is a copy the caller may change without changing what CacheTTL keeps.
func (v *Values) clone() *Values {
	out := *v
	out.Values = make(map[string]string, len(v.Values))
	for k, x := range v.Values {
		out.Values[k] = x
	}
	out.Problems = append([]Problem(nil), v.Problems...)
	out.Leases = append([]string(nil), v.Leases...)
	out.Dynamic = append([]string(nil), v.Dynamic...)
	out.Linked = append([]string(nil), v.Linked...)
	out.Followed = append([]Followed(nil), v.Followed...)
	return &out
}

// cachedValues answers from memory within CacheTTL, else revalidates with the digest (a 304
// keeps the values) or reads them again. noDynamic: read with `dynamic=false`, so naming dynamic
// keys holds no credential.
func (c *Client) cachedValues(ctx context.Context, path string, noDynamic bool) (*Values, error) {
	now := time.Now
	if c.Now != nil {
		now = c.Now
	}
	c.cacheMu.Lock()
	hit, ok := c.cache[path]
	c.cacheMu.Unlock()
	if ok && now().Sub(hit.at) < c.CacheTTL {
		return hit.values, nil
	}
	var out *Values
	err := c.authed(ctx, func(token string) (err error) {
		out, err = c.revalidate(ctx, token, path, hit, ok, now, noDynamic)
		return err
	})
	return out, err
}

// revalidate is one conditional read of path with token: a 304 keeps hit's values.
func (c *Client) revalidate(ctx context.Context, token, path string, hit cached, ok bool, now func() time.Time, noDynamic bool) (*Values, error) {
	sep := "?"
	if strings.Contains(path, "?") {
		sep = "&"
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, strings.TrimRight(c.API, "/")+path+sep+"digest=1", nil)
	if err != nil {
		return nil, err
	}
	if token != "" {
		req.Header.Set("Authorization", "Bearer "+token)
	}
	if ok && hit.etag != "" {
		req.Header.Set("If-None-Match", `"`+hit.etag+`"`)
	}
	h := c.HTTP
	if h == nil {
		h = http.DefaultClient
	}
	res, err := h.Do(req)
	if err != nil {
		return nil, err
	}
	defer res.Body.Close()
	if res.StatusCode == http.StatusNotModified && ok {
		c.cacheMu.Lock()
		hit.at = now()
		c.cache[path] = hit
		c.cacheMu.Unlock()
		return hit.values, nil
	}
	data, err := io.ReadAll(io.LimitReader(res.Body, 16<<20))
	if err != nil {
		return nil, err
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
		return nil, &Error{Status: res.StatusCode, Code: e.Code, Message: e.Error}
	}
	var v Values
	if err := json.Unmarshal(data, &v); err != nil {
		return nil, err
	}
	etag := strings.Trim(strings.TrimPrefix(res.Header.Get("ETag"), "W/"), `"`)
	c.cacheMu.Lock()
	// Never kept: dynamic credentials (they end with their lease; with `dynamic=false` there are
	// none, only their names) or a key that didn't resolve (the digest may not move when the link
	// recovers).
	if (len(v.Dynamic) > 0 && !noDynamic) || len(v.Problems) > 0 {
		delete(c.cache, path)
	} else {
		if c.cache == nil {
			c.cache = map[string]cached{}
		}
		c.cache[path] = cached{values: &v, etag: etag, at: now()}
	}
	c.cacheMu.Unlock()
	return &v, nil
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
		Sealed   Sealed     `json:"sealed"`
		Context  string     `json:"context"`
		Problems []Problem  `json:"problems"`
		Leases   []string   `json:"leases"`
		Dynamic  []string   `json:"dynamic"`
		Linked   []string   `json:"linked"`
		Followed []Followed `json:"followed"`
	}
	if err := c.call(ctx, http.MethodPost, e+"/resolve", map[string]any{"consumer": consumer, "path": orRoot(path), "seal_to": PublicKey(key)}, &s); err != nil {
		return nil, err
	}
	plain, err := Open(s.Sealed, key, s.Context)
	if err != nil {
		return nil, err
	}
	v := Values{Problems: s.Problems, Leases: s.Leases, Dynamic: s.Dynamic, Linked: s.Linked, Followed: s.Followed}
	if err := json.Unmarshal(plain, &v.Values); err != nil {
		return nil, errors.New("the sealed values aren't a map")
	}
	return &v, nil
}

// Digest moves whenever env's values under path would; it reads no value.
func (c *Client) Digest(ctx context.Context, env, path string) (string, error) {
	return c.DigestSince(ctx, env, path, "", 0)
}

// DigestSince long-polls the digest: it answers as soon as it differs from since, or with the same
// one once wait (at most a minute) runs out. A server without long polls answers at once.
func (c *Client) DigestSince(ctx context.Context, env, path, since string, wait time.Duration) (string, error) {
	e, err := c.env(ctx, env)
	if err != nil {
		return "", err
	}
	q := query(path)
	// Under the client's own timeout, and the server's 25 s.
	if c.HTTP != nil && c.HTTP.Timeout > 0 {
		wait = min(wait, c.HTTP.Timeout-5*time.Second)
	}
	if since != "" && wait >= time.Second {
		sep := "?"
		if q != "" {
			sep = "&"
		}
		q += sep + "since=" + url.QueryEscape(since) + "&wait=" + strconv.Itoa(int(min(wait, 25*time.Second)/time.Second))
	}
	var d struct {
		Digest string `json:"digest"`
	}
	return d.Digest, c.call(ctx, http.MethodGet, e+"/digest"+q, nil, &d)
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
		// A long poll: a change arrives within a second or two (Every paces a server without them).
		now, err := c.DigestSince(ctx, env, w.Path, last, 25*time.Second)
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
			v, err = c.freshValues(ctx, env, w.Path)
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
