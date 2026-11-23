// Package secrets is a client for roll secrets (docs/secrets.md, "Consumption"): an environment's
// values, the runtime resolve (sealed to a key only this process holds, if asked), the digest that
// changes when the values would, a watch that follows it, one key's record and stored value, and
// one key's writes. It mirrors @roll/secrets.
//
// A client holds a roll token (`rlt_…`, roll-access's) with a rule on one project, the operator
// token, or the short-lived token a workload identity's login gives (Login, LoginWithIdentity,
// FromEnv).
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
	"sync/atomic"
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
	// again with the answer's digest and every key's stamp (`…/values/delta`): an unchanged folder
	// costs what a 304 does, and a changed one only its changed keys. A folder holding dynamic
	// credentials is never kept (they end with their lease). Zero: off.
	CacheTTL time.Duration
	// CacheSize caps how many folders CacheTTL keeps; past it, the one read longest ago goes. Zero:
	// no cap.
	CacheSize int
	// Now is the clock CacheTTL is measured with (for tests); time.Now when nil.
	Now func() time.Time

	once    sync.Mutex
	project string
	// login renews the token of a client a workload identity's login made.
	login *relogin

	cacheMu sync.Mutex
	cache   map[string]cached
	// noDelta: a rolld without `…/values/delta`; reads are full (conditional, with CacheTTL).
	noDelta atomic.Bool
}

// cached is a Values answer kept by CacheTTL, with its digest and stamps, and when it was asked.
type cached struct {
	held
	at time.Time
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
	// Withheld are sensitive keys left out: only roll's runtimes and syncs read them.
	Withheld []string `json:"withheld,omitempty"`
	// Digest is the folder's digest the values were read against, when the read asked for it (a
	// cached or delta read does).
	Digest string `json:"digest,omitempty"`
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
	q := query(path)
	if o.noDynamic {
		if path == "" {
			q += "?dynamic=false"
		} else {
			q += "&dynamic=false"
		}
	}
	if c.CacheTTL > 0 {
		v, err := c.cachedValues(ctx, e, q, o.noDynamic)
		if err != nil {
			return nil, err
		}
		return v.clone(), nil
	}
	var v Values
	return &v, c.call(ctx, http.MethodGet, e+"/values"+q, nil, &v)
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

// SecretInfo is one key's record as a folder sees it, never its value.
type SecretInfo struct {
	Key     string
	Path    string
	Type    string
	Version int64
	Comment string
	Tags    []string
	// LinkKind is the kind of the link the value comes from (postgres, github…); "" for a value
	// stored in roll.
	LinkKind  string
	Source    Source
	UpdatedAt int64
	UpdatedBy string
	ExpiresAt *int64
}

// Source is where a key's record comes from: Kind "own" (set in the folder itself), "base" (the
// environment's base, Environment) or "import" (an imported folder: Environment and Path, and
// Project when another project shares it).
type Source struct {
	Kind        string `json:"kind"`
	Environment string `json:"environment,omitempty"`
	Path        string `json:"path,omitempty"`
	Project     string `json:"project,omitempty"`
}

// Secret is key's record as path's folder ("" for the root) sees it, without its value: its
// version, comment, link kind and where it's set (Source: its own, a base's or an import's). nil,
// with no error, when the folder has no such key.
func (c *Client) Secret(ctx context.Context, env, path, key string) (*SecretInfo, error) {
	u, err := c.secretURL(ctx, env, path, key)
	if err != nil {
		return nil, err
	}
	var s struct {
		Key     string   `json:"key"`
		Path    string   `json:"path"`
		Type    string   `json:"type"`
		Version int64    `json:"version"`
		Comment string   `json:"comment"`
		Tags    []string `json:"tags"`
		Link    *struct {
			Kind string `json:"kind"`
		} `json:"link"`
		Source    Source `json:"source"`
		UpdatedAt int64  `json:"updated_at"`
		UpdatedBy string `json:"updated_by"`
		ExpiresAt *int64 `json:"expires_at"`
	}
	if err := c.call(ctx, http.MethodGet, u, nil, &s); err != nil {
		var e *Error
		if errors.As(err, &e) && e.Status == http.StatusNotFound {
			return nil, nil
		}
		return nil, err
	}
	info := &SecretInfo{Key: s.Key, Path: s.Path, Type: s.Type, Version: s.Version, Comment: s.Comment, Tags: s.Tags, Source: s.Source, UpdatedAt: s.UpdatedAt, UpdatedBy: s.UpdatedBy, ExpiresAt: s.ExpiresAt}
	if s.Link != nil {
		info.LinkKind = s.Link.Kind
	}
	return info, nil
}

// StoredValue is key's value as stored, references left as written (`resolve=false`). It's a
// reveal: the token needs `reveal`, the read is audited, and CacheTTL never keeps it. A linked
// key's value still comes from its link. A key that doesn't read (sensitive, a link that failed)
// is an *Error with the Code `unresolved`; errors never hold the value.
func (c *Client) StoredValue(ctx context.Context, env, path, key string) (string, error) {
	e, err := c.env(ctx, env)
	if err != nil {
		return "", err
	}
	q := query(path)
	if q == "" {
		q = "?resolve=false"
	} else {
		q += "&resolve=false"
	}
	var r struct {
		Value    *string   `json:"value"`
		Problems []Problem `json:"problems"`
	}
	if err := c.call(ctx, http.MethodGet, e+"/secrets/"+url.PathEscape(key)+"/value"+q, nil, &r); err != nil {
		return "", err
	}
	if len(r.Problems) > 0 {
		return "", &Error{Status: 409, Code: "unresolved", Message: key + " doesn't read: " + r.Problems[0].Problem}
	}
	if r.Value == nil {
		return "", &Error{Status: 502, Message: "the answer for " + key + " has no value"}
	}
	return *r.Value, nil
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

// held is a folder's last answer with every key's stamp, so a read after a change asks only for
// what changed (`…/values/delta`, docs/secrets.md "Delta reads"): what a watch keeps, and what
// CacheTTL keeps.
type held struct {
	values *Values
	digest string
	basis  string
	stamps map[string]string
	// off: the folder's claim is past what rolld takes (a 413); its reads are full.
	off bool
}

// deltaAnswer is `…/values/delta`'s answer: the full set (Delta false), or the keys that changed
// since the answer held, the names it no longer has, and the changed keys' stamps.
type deltaAnswer struct {
	Values
	Delta   bool              `json:"delta"`
	Digest  string            `json:"digest"`
	Basis   string            `json:"basis"`
	Removed []string          `json:"removed"`
	Stamps  map[string]string `json:"stamps"`
}

// errNoRoute is a rolld without `…/values/delta`, and errTooBig a claim past what it takes: both
// read in full instead.
var (
	errNoRoute = errors.New("rolld has no delta reads")
	errTooBig  = errors.New("the claim is past what rolld takes")
)

// readDelta asks u (`…/values/delta?…`) for what changed since the answer h holds and merges it
// in, leaving the new answer in h: the full set when h holds nothing it may claim (nothing yet,
// problems, no basis) or when rolld answers it. On any failure h holds nothing, so the next claim
// asks for the full set.
func (c *Client) readDelta(ctx context.Context, u string, h *held) (*Values, error) {
	claim := map[string]any{}
	if h.values != nil && h.basis != "" && len(h.values.Problems) == 0 {
		claim = map[string]any{"digest": h.digest, "basis": h.basis, "stamps": h.stamps}
	}
	var a deltaAnswer
	if err := c.call(ctx, http.MethodPost, u, claim, &a); err != nil {
		*h = held{}
		var re *Error
		if errors.As(err, &re) {
			switch re.Status {
			case http.StatusNotFound, http.StatusMethodNotAllowed:
				return nil, errNoRoute
			case http.StatusRequestEntityTooLarge:
				return nil, errTooBig
			}
		}
		return nil, err
	}
	stamps := a.Stamps
	if a.Delta && h.values != nil {
		values := make(map[string]string, len(h.values.Values)+len(a.Values.Values))
		stamps = make(map[string]string, len(h.stamps)+len(a.Stamps))
		for k, v := range h.values.Values {
			values[k] = v
		}
		for k, s := range h.stamps {
			stamps[k] = s
		}
		for _, k := range a.Removed {
			delete(values, k)
			delete(stamps, k)
		}
		for k, v := range a.Values.Values {
			values[k] = v
		}
		for k, s := range a.Stamps {
			stamps[k] = s
		}
		a.Values.Values = values
	}
	v := a.Values
	v.Digest = a.Digest
	*h = held{values: &v, digest: a.Digest, basis: a.Basis, stamps: stamps}
	return &v, nil
}

// deltaValues is freshValues through the delta route, keeping the answer in h. Any failure of the
// delta reads in full (a rolld without the route: from then on).
func (c *Client) deltaValues(ctx context.Context, env, path string, h *held) (*Values, error) {
	if !h.off && !c.noDelta.Load() {
		e, err := c.env(ctx, env)
		if err != nil {
			return nil, err
		}
		v, err := c.readDelta(ctx, e+"/values/delta"+query(path), h)
		if err == nil {
			// The newest answer: a kept one for the folder goes.
			c.cacheMu.Lock()
			delete(c.cache, e+"/values"+query(path))
			c.cacheMu.Unlock()
			return v.clone(), nil
		}
		if ctx.Err() != nil {
			return nil, err
		}
		switch {
		case errors.Is(err, errNoRoute):
			c.noDelta.Store(true)
		case errors.Is(err, errTooBig):
			h.off = true
		}
	}
	return c.freshValues(ctx, env, path)
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
	out.Withheld = append([]string(nil), v.Withheld...)
	return &out
}

// cachedValues answers from memory within CacheTTL, else asks for what changed since the kept
// answer (`…/values/delta`; the full set when nothing is kept). Where that fails, a conditional
// read: a 304 keeps the values. e is the environment's route, q the query; noDynamic: read with
// `dynamic=false`, so naming dynamic keys holds no credential.
func (c *Client) cachedValues(ctx context.Context, e, q string, noDynamic bool) (*Values, error) {
	now := time.Now
	if c.Now != nil {
		now = c.Now
	}
	key := e + "/values" + q
	c.cacheMu.Lock()
	hit, ok := c.cache[key]
	c.cacheMu.Unlock()
	if ok && now().Sub(hit.at) < c.CacheTTL {
		return hit.values, nil
	}
	off := hit.off
	if !off && !c.noDelta.Load() {
		h := hit.held
		v, err := c.readDelta(ctx, e+"/values/delta"+q, &h)
		if err == nil {
			c.keep(key, cached{held: h, at: now()}, noDynamic)
			return v, nil
		}
		if ctx.Err() != nil {
			return nil, err
		}
		// Any failure reads in full below.
		switch {
		case errors.Is(err, errNoRoute):
			c.noDelta.Store(true)
		case errors.Is(err, errTooBig):
			off = true
		}
	}
	var out *Values
	err := c.authed(ctx, func(token string) (err error) {
		out, err = c.revalidate(ctx, token, key, hit, ok, now, noDynamic, off)
		return err
	})
	return out, err
}

// keep keeps an answer for CacheTTL, unless it holds dynamic credentials (they end with their
// lease; with `dynamic=false` there are none, only their names) or a key that didn't resolve (the
// digest may not move when the link recovers). Past CacheSize, the folder read longest ago goes.
func (c *Client) keep(key string, k cached, noDynamic bool) {
	c.cacheMu.Lock()
	defer c.cacheMu.Unlock()
	if (len(k.values.Dynamic) > 0 && !noDynamic) || len(k.values.Problems) > 0 {
		delete(c.cache, key)
		return
	}
	if c.cache == nil {
		c.cache = map[string]cached{}
	}
	if _, kept := c.cache[key]; !kept && c.CacheSize > 0 && len(c.cache) >= c.CacheSize {
		oldest := ""
		for k, x := range c.cache {
			if oldest == "" || x.at.Before(c.cache[oldest].at) {
				oldest = k
			}
		}
		delete(c.cache, oldest)
	}
	c.cache[key] = k
}

// revalidate is one conditional read of path with token: a 304 keeps hit's values. off: the
// folder's claims are past what rolld takes, kept with the answer.
func (c *Client) revalidate(ctx context.Context, token, path string, hit cached, ok bool, now func() time.Time, noDynamic, off bool) (*Values, error) {
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
	if ok && hit.digest != "" {
		req.Header.Set("If-None-Match", `"`+hit.digest+`"`)
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
		hit.at, hit.off = now(), off
		c.keep(path, hit, noDynamic)
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
	if v.Digest == "" {
		v.Digest = etag
	}
	c.keep(path, cached{held: held{values: &v, digest: etag, off: off}, at: now()}, noDynamic)
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
	// Consumer, when set, resolves as it (Resolve) instead of reading (Values). `…/resolve` has no
	// delta reads, so each change resolves the whole folder.
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
	var h held
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
			v, err = c.deltaValues(ctx, env, w.Path, &h)
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
