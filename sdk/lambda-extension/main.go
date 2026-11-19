// Command roll-secrets is roll secrets' AWS Lambda extension (docs/secrets.md, "AWS Lambda"), as
// AWS's Parameters and Secrets Lambda Extension: a layer that runs beside a function and answers
// its environments' values on localhost from a cache, so the function holds no roll token and an
// invocation doesn't call roll. It logs in as a workload identity with an OIDC token AWS STS signs
// for the function's own IAM role (GetWebIdentityToken), or with ROLL_TOKEN.
//
// It follows the Lambda Extensions API: register, then wait on each event until SHUTDOWN.
package main

import (
	"context"
	"errors"
	"fmt"
	"log"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"time"
)

var version = "dev"

func main() {
	log.SetFlags(0)
	log.SetPrefix("[roll-secrets] ")
	if err := run(context.Background(), os.Getenv, filepath.Base(os.Args[0]), time.Now); err != nil {
		log.Print(err)
		os.Exit(1)
	}
}

// config is the extension's settings, from the function's environment variables.
type config struct {
	api        string
	port       string
	defaultEnv string
	ttl        time.Duration
	size       int
	prefetch   []folder
}

// folder is one environment's folder.
type folder struct{ env, path string }

func (f folder) String() string { return f.env + ":" + f.path }

func parseConfig(env func(string) string) (config, error) {
	c := config{
		api:        strings.TrimRight(strings.TrimSpace(env("ROLL_API_URL")), "/"),
		port:       "2773",
		defaultEnv: strings.TrimSpace(env("ROLL_ENV")),
		ttl:        300 * time.Second,
		size:       1000,
	}
	if c.api == "" {
		return c, errors.New("ROLL_API_URL isn't set")
	}
	if p := strings.TrimSpace(env("ROLL_EXTENSION_PORT")); p != "" {
		if n, err := strconv.Atoi(p); err != nil || n < 1 || n > 65535 {
			return c, errors.New("ROLL_EXTENSION_PORT is a port number")
		}
		c.port = p
	}
	if t := strings.TrimSpace(env("ROLL_CACHE_TTL")); t != "" {
		d, err := time.ParseDuration(t)
		if n, nerr := strconv.Atoi(t); nerr == nil {
			d, err = time.Duration(n)*time.Second, nil
		}
		if err != nil || d < 0 {
			return c, errors.New("ROLL_CACHE_TTL is seconds (300) or a duration (5m); 0 turns the cache off")
		}
		c.ttl = d
	}
	if s := strings.TrimSpace(env("ROLL_CACHE_SIZE")); s != "" {
		n, err := strconv.Atoi(s)
		if err != nil || n < 0 {
			return c, errors.New("ROLL_CACHE_SIZE is how many folders are kept (0: no cap)")
		}
		c.size = n
	}
	for _, p := range strings.Split(env("ROLL_PREFETCH"), ",") {
		if p = strings.TrimSpace(p); p == "" {
			continue
		}
		e, path, _ := strings.Cut(p, ":")
		if e == "" {
			return c, fmt.Errorf("ROLL_PREFETCH is env:path,…: %q names no environment", p)
		}
		c.prefetch = append(c.prefetch, folder{e, orRoot(path)})
	}
	return c, nil
}

func orRoot(path string) string {
	if path == "" {
		return "/"
	}
	return path
}

// run is the extension's life: register, listen on localhost, load ROLL_PREFETCH, then follow
// events until SHUTDOWN.
func run(ctx context.Context, env func(string) string, name string, now func() time.Time) error {
	runtime := env("AWS_LAMBDA_RUNTIME_API")
	if runtime == "" {
		return errors.New("AWS_LAMBDA_RUNTIME_API isn't set: this runs as a Lambda extension")
	}
	lx := &extensions{base: "http://" + runtime + "/2020-01-01/extension", http: &http.Client{}}
	if err := lx.register(ctx, name); err != nil {
		return err
	}
	cfg, err := parseConfig(env)
	if err != nil {
		lx.fail(ctx, "init", "Config", err)
		return err
	}
	// Bound to the loopback only: values never leave the sandbox.
	ln, err := net.Listen("tcp", net.JoinHostPort("127.0.0.1", cfg.port))
	if err != nil {
		lx.fail(ctx, "init", "Listen", err)
		return err
	}
	session := env("AWS_SESSION_TOKEN")
	if session == "" {
		log.Print("AWS_SESSION_TOKEN isn't set: every request is refused")
	}
	st := newStore(cfg, connector(cfg.api, env, now), now)
	srv := &http.Server{Handler: handler(st, session, cfg.defaultEnv), ReadHeaderTimeout: 5 * time.Second}
	go func() {
		if err := srv.Serve(ln); err != nil && !errors.Is(err, http.ErrServerClosed) {
			log.Print(err)
		}
	}()
	log.Printf("%s listening on 127.0.0.1:%s", version, cfg.port)
	// Before the first event, so the first invocation is answered from memory. Lambda's init phase
	// lasts 10 s in all: what doesn't load in 5 is read on its first request.
	pctx, cancel := context.WithTimeout(ctx, 5*time.Second)
	st.prefetch(pctx, cfg.prefetch)
	cancel()
	for {
		ev, err := lx.next(ctx)
		if err != nil {
			lx.fail(ctx, "exit", "Next", err)
			_ = srv.Close()
			return err
		}
		if ev.EventType == "SHUTDOWN" {
			deadline := time.UnixMilli(ev.DeadlineMs).Add(-100 * time.Millisecond)
			sctx, cancel := context.WithDeadline(ctx, deadline)
			defer cancel()
			if err := srv.Shutdown(sctx); err != nil {
				_ = srv.Close()
			}
			return nil
		}
		// INVOKE: nothing to do; the function asks for what it needs.
	}
}
