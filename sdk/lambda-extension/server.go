package main

import (
	"context"
	"crypto/subtle"
	"encoding/json"
	"errors"
	"fmt"
	"log"
	"net/http"
	"strings"
	"sync"
	"time"

	"github.com/roll-sh/roll/sdk/go/secrets"
)

// store reads folders through one logged-in client. Folders without dynamic credentials are the
// client's to keep (CacheTTL, then a conditional read: a 304 decrypts nothing); a folder holding
// dynamic credentials, which the client never keeps, is kept here until a while before its first
// lease ends, and never past the TTL.
type store struct {
	connect func(context.Context) (*secrets.Client, error)
	ttl     time.Duration
	size    int
	now     func() time.Time
	http    *http.Client

	connMu sync.Mutex
	client *secrets.Client

	mu  sync.Mutex
	dyn map[folder]keptDynamic
}

type keptDynamic struct {
	values *secrets.Values
	until  time.Time
}

func newStore(cfg config, connect func(context.Context) (*secrets.Client, error), now func() time.Time) *store {
	return &store{connect: connect, ttl: cfg.ttl, size: cfg.size, now: now, http: &http.Client{Timeout: 10 * time.Second}, dyn: map[folder]keptDynamic{}}
}

// logIn is the client, logged in on first use (and again after a failure: roll or the identity
// provider may be down for a moment at a cold start).
func (s *store) logIn(ctx context.Context) (*secrets.Client, error) {
	s.connMu.Lock()
	defer s.connMu.Unlock()
	if s.client != nil {
		return s.client, nil
	}
	c, err := s.connect(ctx)
	if err != nil {
		return nil, err
	}
	c.CacheTTL, c.CacheSize, c.Now, c.HTTP = s.ttl, s.size, s.now, s.http
	s.client = c
	return c, nil
}

// values are f's values, from memory when they may be.
func (s *store) values(ctx context.Context, f folder) (*secrets.Values, error) {
	s.mu.Lock()
	k, ok := s.dyn[f]
	if ok && s.now().Before(k.until) {
		s.mu.Unlock()
		return k.values, nil
	}
	delete(s.dyn, f)
	s.mu.Unlock()
	c, err := s.logIn(ctx)
	if err != nil {
		return nil, err
	}
	v, err := c.Values(ctx, f.env, f.path)
	if err != nil {
		return nil, err
	}
	if len(v.Dynamic) > 0 && len(v.Problems) == 0 {
		s.keepDynamic(ctx, c, f, v)
	}
	return v, nil
}

// keepDynamic keeps a folder with dynamic credentials until a fifth of its first lease's life (at
// least a minute) is left, within the TTL. The values answer doesn't say when a lease ends, so
// each lease is renewed once (its link's TTL from now), which does: a lease that can't be renewed
// leaves the folder unkept, read again on its next request.
func (s *store) keepDynamic(ctx context.Context, c *secrets.Client, f folder, v *secrets.Values) {
	if s.ttl <= 0 || len(v.Leases) == 0 {
		return
	}
	now := s.now()
	var ends time.Time
	for _, id := range v.Leases {
		l, err := c.Renew(ctx, id, 0)
		if err != nil {
			log.Printf("%s: not kept: renewing lease %s: %v", f, id, err)
			return
		}
		if e := time.UnixMilli(l.ExpiresAt); ends.IsZero() || e.Before(ends) {
			ends = e
		}
	}
	margin := max(ends.Sub(now)/5, time.Minute)
	until := ends.Add(-margin)
	if ttl := now.Add(s.ttl); ttl.Before(until) {
		until = ttl
	}
	if !until.After(now) {
		return
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	if _, kept := s.dyn[f]; !kept && s.size > 0 && len(s.dyn) >= s.size {
		var soonest folder
		for g, k := range s.dyn {
			if soonest == (folder{}) || k.until.Before(s.dyn[soonest].until) {
				soonest = g
			}
		}
		delete(s.dyn, soonest)
	}
	s.dyn[f] = keptDynamic{values: v, until: until}
}

// prefetch loads folders before the first event, logging (never a value) what didn't load.
func (s *store) prefetch(ctx context.Context, folders []folder) {
	if len(folders) == 0 {
		return
	}
	if _, err := s.logIn(ctx); err != nil {
		log.Printf("prefetch: logging in: %v", err)
		return
	}
	var wg sync.WaitGroup
	for _, f := range folders {
		wg.Add(1)
		go func() {
			defer wg.Done()
			v, err := s.values(ctx, f)
			if err == nil && len(v.Problems) > 0 {
				err = unresolved(f, v)
			}
			if err != nil {
				log.Printf("prefetch %s: %v", f, err)
			}
		}()
	}
	wg.Wait()
}

// handler answers the function. Every request carries the function's own AWS_SESSION_TOKEN in
// X-Roll-Token (as AWS's extension asks for X-Aws-Parameters-Secrets-Token), so other code in the
// sandbox that can reach localhost but doesn't have the function's credentials reads nothing.
func handler(s *store, session, defaultEnv string) http.Handler {
	mux := http.NewServeMux()
	where := func(r *http.Request) (folder, error) {
		q := r.URL.Query()
		f := folder{env: q.Get("env"), path: orRoot(q.Get("path"))}
		if f.env == "" {
			f.env = defaultEnv
		}
		if f.env == "" {
			return f, errors.New("name the environment: ?env=production (or set ROLL_ENV)")
		}
		return f, nil
	}
	read := func(w http.ResponseWriter, r *http.Request) (folder, *secrets.Values, bool) {
		f, err := where(r)
		if err != nil {
			answerError(w, http.StatusBadRequest, "", err.Error())
			return f, nil, false
		}
		ctx, cancel := context.WithTimeout(r.Context(), 15*time.Second)
		defer cancel()
		v, err := s.values(ctx, f)
		if err != nil {
			status, code := http.StatusBadGateway, ""
			var e *secrets.Error
			if errors.As(err, &e) {
				code = e.Code
				switch e.Status {
				case http.StatusForbidden, http.StatusNotFound, http.StatusConflict, http.StatusLocked, http.StatusTooManyRequests:
					status = e.Status
				}
			}
			// roll's errors and the client's name keys and folders, never values.
			log.Printf("%s %s: %v", r.URL.Path, f, err)
			answerError(w, status, code, fmt.Sprintf("reading %s: %v", f, err))
			return f, nil, false
		}
		return f, v, true
	}
	mux.HandleFunc("GET /secrets", func(w http.ResponseWriter, r *http.Request) {
		f, v, ok := read(w, r)
		if !ok {
			return
		}
		if len(v.Problems) > 0 {
			answerError(w, http.StatusConflict, "unresolved", unresolved(f, v).Error())
			return
		}
		w.Header().Set("Content-Type", "application/json")
		_ = json.NewEncoder(w).Encode(v.Values)
	})
	mux.HandleFunc("GET /secrets/{key}", func(w http.ResponseWriter, r *http.Request) {
		f, v, ok := read(w, r)
		if !ok {
			return
		}
		key := r.PathValue("key")
		for _, p := range v.Problems {
			if p.Key == key {
				answerError(w, http.StatusConflict, "unresolved", fmt.Sprintf("%s in %s doesn't resolve: %s", key, f, p.Problem))
				return
			}
		}
		value, found := v.Values[key]
		if !found {
			answerError(w, http.StatusNotFound, "not_found", fmt.Sprintf("%s has no key %s", f, key))
			return
		}
		w.Header().Set("Content-Type", "text/plain; charset=utf-8")
		_, _ = w.Write([]byte(value))
	})
	mux.HandleFunc("GET /health", func(w http.ResponseWriter, r *http.Request) {
		ctx, cancel := context.WithTimeout(r.Context(), 15*time.Second)
		defer cancel()
		if _, err := s.logIn(ctx); err != nil {
			answerError(w, http.StatusServiceUnavailable, "login", "not logged in to roll: "+err.Error())
			return
		}
		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write([]byte(`{"status":"ok","version":"` + version + `"}` + "\n"))
	})
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Cache-Control", "no-store")
		got := r.Header.Get("X-Roll-Token")
		if session == "" || subtle.ConstantTimeCompare([]byte(got), []byte(session)) != 1 {
			answerError(w, http.StatusUnauthorized, "unauthorized", "send the function's AWS_SESSION_TOKEN in X-Roll-Token")
			return
		}
		mux.ServeHTTP(w, r)
	})
}

func unresolved(f folder, v *secrets.Values) error {
	keys := make([]string, 0, len(v.Problems))
	for _, p := range v.Problems {
		keys = append(keys, p.Key+" ("+p.Problem+")")
	}
	return fmt.Errorf("%s doesn't resolve: %s", f, strings.Join(keys, ", "))
}

func answerError(w http.ResponseWriter, status int, code, message string) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	_ = json.NewEncoder(w).Encode(map[string]string{"error": message, "code": code})
}
