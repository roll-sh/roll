package main

// Mount: a SecretProviderClass's parameters say which project, environment and folder, and how
// the pod logs in; the answer is files the driver writes into the pod's volume. Between mounts
// only a login's recent answers are kept, in memory, for that login alone (answerCache), and no
// value is ever logged or put in an error.

import (
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"regexp"
	"sort"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/roll-sh/roll/sdk/go/secrets"
)

// provider serves Mount for the driver.
type provider struct {
	http    *http.Client
	version string
	// apis are the API URLs a SecretProviderClass may name (the provider's -api); empty: any.
	apis []string
	// allowHTTP lets them be plain http:// (-allow-http, for a cluster-local rolld only): a pod's
	// token otherwise goes only over TLS.
	allowHTTP bool
	// logins are pods' recent logins, by their service account token (hashed): a pod's rotation
	// polls and its siblings' mounts don't each log in (an identity takes 120 logins a minute).
	logins loginCache
	// answers are those logins' recent answers, so a rotation poll reads only what changed.
	answers answerCache
}

// answerCache keeps, per login (a pod's service account token or a node's roll token, hashed) and
// project, a client whose cache holds the folders that login's mounts read, with every key's
// stamp. Every mount still asks roll, for what changed since (`…/values/delta`, docs/secrets.md
// "Delta reads"): an unchanged folder decrypts nothing, a changed one only its changed keys, and
// roll answers the full set whenever it can't prove the delta exact. An answer is never used for
// another login: each pod logs in as its own service account. An entry unused for answersKeep
// goes, and at most answersMax are kept.
type answerCache struct {
	mu sync.Mutex
	m  map[string]*answers
}

type answers struct {
	mu   sync.Mutex // one mount at a time: the client's token is set for each
	c    *secrets.Client
	used time.Time
}

const (
	answersKeep = loginReuse
	answersMax  = 4096
	// The folders one login's mounts read.
	answersFolders = 16
)

func (a *answerCache) get(key, api string, h *http.Client, now time.Time) *answers {
	a.mu.Lock()
	defer a.mu.Unlock()
	if a.m == nil {
		a.m = map[string]*answers{}
	}
	// A mount holding an entry that goes keeps using it; the next one starts afresh.
	for k, x := range a.m {
		if now.Sub(x.used) >= answersKeep {
			delete(a.m, k)
		}
	}
	e := a.m[key]
	if e == nil {
		if len(a.m) >= answersMax {
			a.m = map[string]*answers{}
		}
		// CacheTTL only turns the cache on: every mount asks roll.
		e = &answers{c: &secrets.Client{API: api, HTTP: h, CacheTTL: time.Nanosecond, CacheSize: answersFolders}}
		a.m[key] = e
	}
	e.used = now
	return e
}

// loginCache keeps a login's roll token for loginReuse after it was made, at most loginsMax of
// them. A token the server refuses (revoked, ended early) is dropped and the pod logs in again.
type loginCache struct {
	mu sync.Mutex
	m  map[string]cachedLogin
}

type cachedLogin struct {
	token, project string
	at             time.Time
}

const (
	loginReuse = 5 * time.Minute
	loginsMax  = 4096
)

func loginKey(api, identity, saToken string) string {
	h := sha256.Sum256([]byte(api + "\x00" + identity + "\x00" + saToken))
	return hex.EncodeToString(h[:])
}

func (l *loginCache) get(key string, now time.Time) (cachedLogin, bool) {
	l.mu.Lock()
	defer l.mu.Unlock()
	c, ok := l.m[key]
	return c, ok && now.Sub(c.at) < loginReuse
}

func (l *loginCache) put(key string, c cachedLogin) {
	l.mu.Lock()
	defer l.mu.Unlock()
	if l.m == nil {
		l.m = map[string]cachedLogin{}
	}
	if len(l.m) >= loginsMax {
		for k, v := range l.m {
			if c.at.Sub(v.at) >= loginReuse {
				delete(l.m, k)
			}
		}
		if len(l.m) >= loginsMax {
			l.m = map[string]cachedLogin{}
		}
	}
	l.m[key] = c
}

func (l *loginCache) forget(key string) {
	l.mu.Lock()
	defer l.mu.Unlock()
	delete(l.m, key)
}

// errRefused is the server refusing a mount's roll token.
var errRefused = errors.New("roll refused the token")

// params are a SecretProviderClass's `parameters` (docs/secrets.md, "Kubernetes").
type params struct {
	api, identity, audience, project, environment, path string
	// keys: which keys, each `KEY` or `KEY=file`; empty: every key in the folder.
	keys []target
	// format: "files" (one per key), "dotenv" or "json" (one file, named file).
	format, file string
	// audience: the service account token's (the CSIDriver's tokenRequests), default the API's URL.
	// dynamic: include dynamic credentials (their leases aren't renewed: each ends with its TTL).
	dynamic bool
}

type target struct{ key, file string }

var (
	keyName  = regexp.MustCompile(`^[A-Za-z_][A-Za-z0-9_]{0,255}$`)
	fileName = regexp.MustCompile(`^[A-Za-z0-9_][A-Za-z0-9._-]{0,254}$|^\.[A-Za-z0-9_][A-Za-z0-9._-]{0,253}$`)
)

func (s *provider) parse(attrs map[string]string) (params, error) {
	p, err := parse(attrs)
	if err != nil {
		return p, err
	}
	if !s.allowHTTP && !strings.HasPrefix(p.api, "https://") {
		return p, errors.New("parameters.api is https:// (a pod's token goes only over TLS; the provider's -allow-http allows a cluster-local http:// rolld)")
	}
	if len(s.apis) > 0 && !contains(s.apis, p.api) {
		return p, fmt.Errorf("parameters.api %s isn't one this provider serves (its -api)", p.api)
	}
	return p, nil
}

func parse(attrs map[string]string) (params, error) {
	p := params{
		api:         strings.TrimRight(strings.TrimSpace(attrs["api"]), "/"),
		identity:    strings.TrimSpace(attrs["identity"]),
		audience:    strings.TrimSpace(attrs["audience"]),
		project:     strings.TrimSpace(attrs["project"]),
		environment: strings.TrimSpace(attrs["environment"]),
		path:        strings.TrimSpace(attrs["path"]),
		format:      strings.TrimSpace(attrs["format"]),
		file:        strings.TrimSpace(attrs["file"]),
	}
	u, err := url.Parse(p.api)
	if p.api == "" || err != nil || (u.Scheme != "https" && u.Scheme != "http") || u.Host == "" {
		return p, errors.New("parameters.api is roll's API URL (https://…)")
	}
	if p.environment == "" {
		return p, errors.New("parameters.environment is required")
	}
	if p.path == "" {
		p.path = "/"
	}
	// The audience the operator's and `roll run`'s tokens carry: the API's URL.
	if p.audience == "" {
		p.audience = p.api
	}
	switch p.format {
	case "", "files":
		p.format = "files"
	case "dotenv":
		if p.file == "" {
			p.file = ".env"
		}
	case "json":
		if p.file == "" {
			p.file = "secrets.json"
		}
	default:
		return p, fmt.Errorf("parameters.format is files, dotenv or json, not %q", p.format)
	}
	if p.format != "files" && !fileName.MatchString(p.file) {
		return p, fmt.Errorf("parameters.file %q isn't a file name", p.file)
	}
	if d := strings.TrimSpace(attrs["dynamic"]); d != "" {
		if p.dynamic, err = strconv.ParseBool(d); err != nil {
			return p, errors.New("parameters.dynamic is true or false")
		}
	}
	for _, k := range strings.FieldsFunc(attrs["keys"], func(r rune) bool { return r == ',' || r == '\n' || r == ' ' }) {
		key, f, renamed := strings.Cut(k, "=")
		if !renamed {
			f = key
		}
		if !keyName.MatchString(key) {
			return p, fmt.Errorf("parameters.keys: %q isn't a key", key)
		}
		if p.format == "files" && !fileName.MatchString(f) {
			return p, fmt.Errorf("parameters.keys: %q isn't a file name", f)
		}
		p.keys = append(p.keys, target{key, f})
	}
	return p, nil
}

// answer is the values route's answer, as far as a mount reads it.
type answer struct {
	Values   map[string]string `json:"values"`
	Dynamic  []string          `json:"dynamic"`
	Withheld []string          `json:"withheld"`
	Digest   string            `json:"digest"`
}

func (s *provider) Version(_ context.Context, _ *versionRequest) (*versionResponse, error) {
	return &versionResponse{Version: "v1alpha1", RuntimeName: "roll", RuntimeVersion: s.version}, nil
}

func (s *provider) Mount(ctx context.Context, req *mountRequest) (*mountResponse, error) {
	var attrs, secretsRef map[string]string
	if err := json.Unmarshal([]byte(req.Attributes), &attrs); err != nil {
		return nil, errors.New("the driver's attributes aren't JSON")
	}
	if req.Secrets != "" {
		if err := json.Unmarshal([]byte(req.Secrets), &secretsRef); err != nil {
			return nil, errors.New("nodePublishSecretRef isn't readable")
		}
	}
	var mode int32 = 0o644
	if req.Permission != "" {
		m, err := strconv.ParseInt(req.Permission, 10, 32)
		if err != nil {
			return nil, errors.New("the driver's file permission isn't a number")
		}
		mode = int32(m)
	}
	p, err := s.parse(attrs)
	if err != nil {
		return nil, err
	}
	// The login and the read together, bounded (the driver's own deadline may be longer).
	ctx, cancel := context.WithTimeout(ctx, 30*time.Second)
	defer cancel()
	var a *answer
	for attempt := 0; ; attempt++ {
		c, key, err := s.client(ctx, p, attrs, secretsRef, attempt > 0)
		if err != nil {
			return nil, err
		}
		// Whose answers these are: the pod's login, or the node's token.
		who := key
		if who == "" {
			who = loginKey(p.api, "", c.Token)
		}
		project := p.project
		if project == "" {
			if project, err = c.ProjectID(ctx); err != nil {
				return nil, fmt.Errorf("finding the project: %w", err)
			}
		}
		a, err = s.values(ctx, c, project, p, who)
		// A remembered login the server refuses: log in again, once.
		if errors.Is(err, errRefused) && key != "" && attempt == 0 {
			s.logins.forget(key)
			continue
		}
		if err != nil {
			return nil, err
		}
		break
	}
	files, err := render(p, a, mode)
	if err != nil {
		return nil, err
	}
	out := &mountResponse{Files: files}
	for _, f := range files {
		out.ObjectVersion = append(out.ObjectVersion, objectVersion{ID: f.Path, Version: a.Digest})
	}
	return out, nil
}

// client logs the pod in: its own service account token through a workload identity (a recent
// login with the same token is reused unless `fresh`; its cache key), else a roll token from
// nodePublishSecretRef (`token`).
func (s *provider) client(ctx context.Context, p params, attrs, ref map[string]string, fresh bool) (*secrets.Client, string, error) {
	if p.identity != "" {
		var tokens map[string]struct {
			Token string `json:"token"`
		}
		_ = json.Unmarshal([]byte(attrs["csi.storage.k8s.io/serviceAccount.tokens"]), &tokens)
		t := tokens[p.audience].Token
		if t == "" {
			return nil, "", fmt.Errorf("the driver passed no service account token for audience %q: add it to the CSIDriver's tokenRequests (the driver chart's tokenRequests[0].audience=%s)", p.audience, p.audience)
		}
		key := loginKey(p.api, p.identity, t)
		if l, ok := s.logins.get(key, time.Now()); ok && !fresh {
			return &secrets.Client{API: p.api, Token: l.token, Project: l.project, HTTP: s.http}, key, nil
		}
		c, err := secrets.Login(ctx, p.api, p.identity, t)
		if err != nil {
			return nil, "", fmt.Errorf("logging in as %s: %w", p.identity, err)
		}
		s.logins.put(key, cachedLogin{token: c.Token, project: c.Project, at: time.Now()})
		c.HTTP = s.http
		return c, key, nil
	}
	if t := strings.TrimSpace(ref["token"]); t != "" {
		c := secrets.New(p.api, t)
		c.HTTP = s.http
		return c, "", nil
	}
	return nil, "", errors.New("set parameters.identity (the pod's service account logs in) or nodePublishSecretRef with a `token`")
}

// values reads the folder strictly: a key that doesn't resolve fails the mount (the pod keeps
// what it had on a rotation) instead of a file with a hole in it. Without dynamic credentials
// (which are issued afresh for each mount) it reads what changed since who's last answer.
func (s *provider) values(ctx context.Context, c *secrets.Client, project string, p params, who string) (*answer, error) {
	if !p.dynamic {
		if a, err := s.delta(ctx, c, project, p, who); a != nil || err != nil {
			return a, err
		}
	}
	return s.strict(ctx, c, project, p)
}

// delta reads the folder through who's kept answers (answerCache). An answer with a key that
// didn't resolve is nil: the strict read says which, and fails the mount.
func (s *provider) delta(ctx context.Context, c *secrets.Client, project string, p params, who string) (*answer, error) {
	e := s.answers.get(who+"\x00"+project, p.api, s.http, time.Now())
	e.mu.Lock()
	defer e.mu.Unlock()
	e.c.Token, e.c.Project = c.Token, project
	v, err := e.c.Values(ctx, p.environment, p.path, secrets.WithoutDynamic())
	if err != nil {
		var re *secrets.Error
		if errors.As(err, &re) && re.Status == http.StatusUnauthorized {
			return nil, fmt.Errorf("reading %s:%s: %w", p.environment, p.path, errRefused)
		}
		// roll's errors name keys, never values.
		return nil, fmt.Errorf("reading %s:%s: %w", p.environment, p.path, err)
	}
	if len(v.Problems) > 0 {
		return nil, nil
	}
	return &answer{Values: v.Values, Dynamic: v.Dynamic, Withheld: v.Withheld, Digest: v.Digest}, nil
}

// strict is one strict read of the folder: nothing is answered unless every key resolves.
func (s *provider) strict(ctx context.Context, c *secrets.Client, project string, p params) (*answer, error) {
	q := url.Values{"path": {p.path}, "strict": {"1"}, "digest": {"1"}, "dynamic": {strconv.FormatBool(p.dynamic)}}
	u := fmt.Sprintf("%s/v1/secrets/projects/%s/environments/%s/values?%s", p.api, url.PathEscape(project), url.PathEscape(p.environment), q.Encode())
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, u, nil)
	if err != nil {
		return nil, err
	}
	req.Header.Set("Authorization", "Bearer "+c.Token)
	res, err := s.http.Do(req)
	if err != nil {
		return nil, fmt.Errorf("reading %s:%s: %w", p.environment, p.path, err)
	}
	defer res.Body.Close()
	body, err := io.ReadAll(io.LimitReader(res.Body, 16<<20))
	if err != nil {
		return nil, err
	}
	if res.StatusCode == http.StatusUnauthorized {
		return nil, fmt.Errorf("reading %s:%s: %w", p.environment, p.path, errRefused)
	}
	if res.StatusCode != http.StatusOK {
		// roll's errors name keys, never values.
		var e struct {
			Error string `json:"error"`
		}
		_ = json.Unmarshal(body, &e)
		if e.Error == "" {
			e.Error = res.Status
		}
		return nil, fmt.Errorf("reading %s:%s: %s", p.environment, p.path, e.Error)
	}
	var a answer
	if err := json.Unmarshal(body, &a); err != nil {
		return nil, errors.New("roll's answer isn't JSON")
	}
	return &a, nil
}

// render turns the values into the files the driver writes.
func render(p params, a *answer, mode int32) ([]file, error) {
	targets := p.keys
	if len(targets) == 0 {
		for k := range a.Values {
			targets = append(targets, target{k, k})
		}
		sort.Slice(targets, func(i, j int) bool { return targets[i].key < targets[j].key })
	}
	picked := map[string]string{}
	for _, t := range targets {
		v, ok := a.Values[t.key]
		if !ok {
			why := "isn't set there"
			switch {
			case contains(a.Dynamic, t.key):
				why = `is a dynamic credential: set parameters.dynamic: "true" (its lease isn't renewed: it ends with its TTL)`
			case contains(a.Withheld, t.key):
				why = "is sensitive, and this login isn't a runtime's"
			}
			return nil, fmt.Errorf("%s %s (%s:%s)", t.key, why, p.environment, p.path)
		}
		picked[t.key] = v
	}
	switch p.format {
	case "dotenv":
		var b bytes.Buffer
		for _, t := range targets {
			b.WriteString(t.key + "=" + dotenvQuote(picked[t.key]) + "\n")
		}
		return []file{{Path: p.file, Mode: mode, Contents: b.Bytes()}}, nil
	case "json":
		b, err := json.MarshalIndent(picked, "", "  ")
		if err != nil {
			return nil, err
		}
		return []file{{Path: p.file, Mode: mode, Contents: append(b, '\n')}}, nil
	}
	seen := map[string]bool{}
	var files []file
	for _, t := range targets {
		if seen[t.file] {
			return nil, fmt.Errorf("parameters.keys: two keys go to %s", t.file)
		}
		seen[t.file] = true
		files = append(files, file{Path: t.file, Mode: mode, Contents: []byte(picked[t.key])})
	}
	return files, nil
}

// dotenvQuote is a value as roll's own dotenv export writes it (roll-secrets' dotenv.rs `dq`):
// as is when it's plain, else in double quotes with backslashes, quotes, dollars and line breaks
// escaped.
func dotenvQuote(v string) string {
	plain := v != ""
	for _, c := range v {
		if !(c < 128 && (c >= 'a' && c <= 'z' || c >= 'A' && c <= 'Z' || c >= '0' && c <= '9' || strings.ContainsRune("-_./:@+,=%", c))) {
			plain = false
			break
		}
	}
	if plain {
		return v
	}
	r := strings.NewReplacer(`\`, `\\`, `"`, `\"`, "$", `\$`, "\n", `\n`, "\r", `\r`)
	return `"` + r.Replace(v) + `"`
}

func contains(list []string, s string) bool {
	for _, x := range list {
		if x == s {
			return true
		}
	}
	return false
}
