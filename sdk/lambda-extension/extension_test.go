package main

import (
	"context"
	"encoding/json"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"net/url"
	"strconv"
	"strings"
	"sync"
	"testing"
	"time"
)

// clock is the time the extension's cache is measured with.
type clock struct {
	mu sync.Mutex
	t  time.Time
}

func (c *clock) now() time.Time {
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.t
}

func (c *clock) add(d time.Duration) {
	c.mu.Lock()
	c.t = c.t.Add(d)
	c.mu.Unlock()
}

// roll is a fake rolld: a workload identity's login, a folder's values with their digest (a 304
// when unchanged), and a folder with a dynamic credential whose lease renews to leaseEnd.
type roll struct {
	mu       sync.Mutex
	logins   int
	reads    map[string]int // full answers, by environment
	notMod   int            // 304s
	renews   int
	value    string
	digest   string
	leaseEnd time.Time
}

func fakeRoll(t *testing.T) (*httptest.Server, *roll) {
	st := &roll{reads: map[string]int{}, value: "postgres://fake-user:fake-pass@db.test/app", digest: "d1"}
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		st.mu.Lock()
		defer st.mu.Unlock()
		answer := func(status int, v any) {
			w.Header().Set("Content-Type", "application/json")
			w.WriteHeader(status)
			_ = json.NewEncoder(w).Encode(v)
		}
		if r.URL.Path == "/v1/secrets/login" {
			var b struct{ Identity, Token string }
			_ = json.NewDecoder(r.Body).Decode(&b)
			if b.Identity != "idn_fn" || b.Token != "jwt-from-sts" {
				answer(401, map[string]string{"error": "the token isn't the identity's"})
				return
			}
			st.logins++
			answer(200, map[string]any{"token": "rlt_workload", "project": "prj_1", "expires_at": time.Now().Add(time.Hour).UnixMilli()})
			return
		}
		if a := r.Header.Get("Authorization"); a != "Bearer rlt_workload" && a != "Bearer rlt_static" {
			answer(401, map[string]string{"error": "unknown token"})
			return
		}
		switch r.URL.Path {
		case "/v1/secrets/projects/prj_1/environments/production/values":
			w.Header().Set("ETag", `"`+st.digest+`"`)
			if r.Header.Get("If-None-Match") == `"`+st.digest+`"` {
				st.notMod++
				w.WriteHeader(http.StatusNotModified)
				return
			}
			st.reads["production"]++
			answer(200, map[string]any{"values": map[string]string{"DATABASE_URL": st.value, "LOG_LEVEL": "debug"}, "problems": []any{}, "leases": []string{}, "dynamic": []string{}, "linked": []string{}})
		case "/v1/secrets/projects/prj_1/environments/staging/values":
			st.reads["staging"]++
			answer(200, map[string]any{"values": map[string]string{"DB_PASSWORD": "fake-dynamic-pw"}, "problems": []any{}, "leases": []string{"lse_1"}, "dynamic": []string{"DB_PASSWORD"}, "linked": []string{}})
		case "/v1/secrets/projects/prj_1/leases/lse_1/renew":
			st.renews++
			answer(200, map[string]any{"id": "lse_1", "expires_at": st.leaseEnd.UnixMilli()})
		default:
			answer(404, map[string]string{"error": "no route " + r.URL.Path})
		}
	}))
	t.Cleanup(srv.Close)
	return srv, st
}

// fakeSTS answers GetWebIdentityToken for a request signed with the function's credentials.
func fakeSTS(t *testing.T, audience string) *httptest.Server {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		body, _ := io.ReadAll(r.Body)
		form, _ := url.ParseQuery(string(body))
		auth := r.Header.Get("Authorization")
		if !strings.HasPrefix(auth, "AWS4-HMAC-SHA256 Credential=AKIDFAKE/") || !strings.Contains(auth, "/us-east-1/sts/aws4_request, SignedHeaders=content-type;host;x-amz-date;x-amz-security-token, Signature=") ||
			r.Header.Get("X-Amz-Security-Token") != "fake-session-token" {
			w.WriteHeader(403)
			_, _ = io.WriteString(w, `<ErrorResponse><Error><Code>SignatureDoesNotMatch</Code><Message>bad signature</Message></Error></ErrorResponse>`)
			return
		}
		if form.Get("Action") != "GetWebIdentityToken" || form.Get("Audience.member.1") != audience || form.Get("SigningAlgorithm") != "RS256" {
			t.Errorf("GetWebIdentityToken asked with %v", form)
		}
		_, _ = io.WriteString(w, `<GetWebIdentityTokenResponse xmlns="https://sts.amazonaws.com/doc/2011-06-15/"><GetWebIdentityTokenResult><WebIdentityToken>jwt-from-sts</WebIdentityToken><Expiration>2026-11-16T12:01:00Z</Expiration></GetWebIdentityTokenResult></GetWebIdentityTokenResponse>`)
	}))
	t.Cleanup(srv.Close)
	return srv
}

// lambdaAPI is a fake Extensions API: it hands out the events the test sends, and tells the test
// each time the extension waits for one.
type lambdaAPI struct {
	events  chan event
	waiting chan struct{}
	stop    chan struct{} // closed when the test ends: a waiting event/next lets go
	name    string
}

func fakeLambda(t *testing.T) (*httptest.Server, *lambdaAPI) {
	l := &lambdaAPI{events: make(chan event), waiting: make(chan struct{}, 8), stop: make(chan struct{})}
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		switch r.URL.Path {
		case "/2020-01-01/extension/register":
			l.name = r.Header.Get("Lambda-Extension-Name")
			w.Header().Set("Lambda-Extension-Identifier", "ext-1")
			_, _ = io.WriteString(w, `{"functionName":"orders","functionVersion":"$LATEST","handler":"index.handler"}`)
		case "/2020-01-01/extension/event/next":
			if r.Header.Get("Lambda-Extension-Identifier") != "ext-1" {
				w.WriteHeader(403)
				return
			}
			l.waiting <- struct{}{}
			select {
			case ev := <-l.events:
				_ = json.NewEncoder(w).Encode(ev)
			case <-l.stop:
			}
		case "/2020-01-01/extension/init/error", "/2020-01-01/extension/exit/error":
			w.WriteHeader(http.StatusAccepted)
		default:
			t.Errorf("the extension called %s %s", r.Method, r.URL.Path)
			w.WriteHeader(404)
		}
	}))
	t.Cleanup(srv.Close)
	t.Cleanup(func() { close(l.stop) }) // before Close, which waits for event/next
	return srv, l
}

func freePort(t *testing.T) string {
	l, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer l.Close()
	return strconv.Itoa(l.Addr().(*net.TCPAddr).Port)
}

func wait(t *testing.T, c <-chan struct{}, what string) {
	t.Helper()
	select {
	case <-c:
	case <-time.After(10 * time.Second):
		t.Fatalf("timed out waiting for %s", what)
	}
}

// register, prefetch, INVOKE and SHUTDOWN, with a login through STS: the function's own role signs
// in, the first request is answered from memory, the cache revalidates after its TTL, and only
// the function's session token reads anything.
func TestTheExtensionsLife(t *testing.T) {
	rolld, st := fakeRoll(t)
	sts := fakeSTS(t, rolld.URL)
	lambda, l := fakeLambda(t)
	port := freePort(t)
	env := map[string]string{
		"AWS_LAMBDA_RUNTIME_API": strings.TrimPrefix(lambda.URL, "http://"),
		"AWS_ACCESS_KEY_ID":      "AKIDFAKE",
		"AWS_SECRET_ACCESS_KEY":  "fake-secret-access-key",
		"AWS_SESSION_TOKEN":      "fake-session-token",
		"AWS_REGION":             "us-east-1",
		"AWS_ENDPOINT_URL_STS":   sts.URL,
		"ROLL_API_URL":           rolld.URL,
		"ROLL_IDENTITY":          "idn_fn",
		"ROLL_EXTENSION_PORT":    port,
		"ROLL_PREFETCH":          "production:/",
	}
	clk := &clock{t: time.Unix(1_800_000_000, 0)}
	done := make(chan error, 1)
	go func() {
		done <- run(context.Background(), func(k string) string { return env[k] }, "roll-secrets", clk.now)
	}()

	wait(t, l.waiting, "the first event/next")
	st.mu.Lock()
	logins, reads := st.logins, st.reads["production"]
	st.mu.Unlock()
	if l.name != "roll-secrets" || logins != 1 || reads != 1 {
		t.Fatalf("before the first event: registered as %q, %d logins, %d reads", l.name, logins, reads)
	}
	l.events <- event{EventType: "INVOKE", RequestID: "req-1", DeadlineMs: time.Now().Add(time.Minute).UnixMilli()}
	wait(t, l.waiting, "event/next after INVOKE")

	base := "http://127.0.0.1:" + port
	get := func(path, token string) (int, string) {
		t.Helper()
		req, _ := http.NewRequest(http.MethodGet, base+path, nil)
		if token != "" {
			req.Header.Set("X-Roll-Token", token)
		}
		res, err := http.DefaultClient.Do(req)
		if err != nil {
			t.Fatal(err)
		}
		defer res.Body.Close()
		b, _ := io.ReadAll(res.Body)
		return res.StatusCode, string(b)
	}
	for _, token := range []string{"", "someone-elses-token"} {
		if code, body := get("/secrets?env=production", token); code != 401 || strings.Contains(body, "fake-user") {
			t.Fatalf("X-Roll-Token %q: %d %s", token, code, body)
		}
	}
	if code, body := get("/secrets/DATABASE_URL?env=production&path=/", "fake-session-token"); code != 200 || body != "postgres://fake-user:fake-pass@db.test/app" {
		t.Fatalf("one key: %d %q", code, body)
	}
	code, body := get("/secrets?env=production", "fake-session-token")
	var all map[string]string
	if err := json.Unmarshal([]byte(body), &all); code != 200 || err != nil || all["LOG_LEVEL"] != "debug" {
		t.Fatalf("the folder: %d %s", code, body)
	}
	counts := func() (int, int) {
		st.mu.Lock()
		defer st.mu.Unlock()
		return st.reads["production"], st.notMod
	}
	if r, n := counts(); r != 1 || n != 0 {
		t.Fatalf("within the TTL, from memory: %d reads, %d revalidations", r, n)
	}
	clk.add(301 * time.Second)
	if code, body := get("/secrets/DATABASE_URL?env=production", "fake-session-token"); code != 200 || body != "postgres://fake-user:fake-pass@db.test/app" {
		t.Fatalf("after the TTL: %d %q", code, body)
	}
	if r, n := counts(); r != 1 || n != 1 {
		t.Fatalf("after the TTL, a 304: %d reads, %d revalidations", r, n)
	}
	st.mu.Lock()
	st.value, st.digest = "postgres://fake-user:rotated@db.test/app", "d2"
	st.mu.Unlock()
	clk.add(301 * time.Second)
	if _, body := get("/secrets/DATABASE_URL?env=production", "fake-session-token"); body != "postgres://fake-user:rotated@db.test/app" {
		t.Fatalf("a change after the TTL: %q", body)
	}
	if code, _ := get("/secrets/NOPE?env=production", "fake-session-token"); code != 404 {
		t.Fatalf("a missing key: %d", code)
	}

	l.events <- event{EventType: "SHUTDOWN", ShutdownReason: "spindown", DeadlineMs: time.Now().Add(2 * time.Second).UnixMilli()}
	select {
	case err := <-done:
		if err != nil {
			t.Fatalf("SHUTDOWN: %v", err)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("still running after SHUTDOWN")
	}
	if _, err := http.Get(base + "/health"); err == nil {
		t.Fatal("still listening after SHUTDOWN")
	}
}

// A folder holding a dynamic credential is kept until a while before its lease ends, never
// beyond, and never past the TTL.
func TestDynamicCredentialsAreKeptWithinTheirLease(t *testing.T) {
	rolld, st := fakeRoll(t)
	clk := &clock{t: time.Unix(1_800_000_000, 0)}
	env := map[string]string{"ROLL_TOKEN": "rlt_static", "ROLL_PROJECT": "prj_1"}
	s := newStore(config{ttl: 5 * time.Minute}, connector(rolld.URL, func(k string) string { return env[k] }, clk.now), clk.now)
	ctx := context.Background()
	read := func() {
		t.Helper()
		v, err := s.values(ctx, folder{"staging", "/"})
		if err != nil || v.Values["DB_PASSWORD"] != "fake-dynamic-pw" {
			t.Fatalf("%v %v", v, err)
		}
	}
	reads := func() int {
		st.mu.Lock()
		defer st.mu.Unlock()
		return st.reads["staging"]
	}
	// The lease ends in 3 minutes: kept for 2 (a minute's margin), not the TTL's 5.
	st.mu.Lock()
	st.leaseEnd = clk.now().Add(3 * time.Minute)
	st.mu.Unlock()
	read()
	clk.add(90 * time.Second)
	read()
	if reads() != 1 {
		t.Fatalf("within the lease, from memory: %d reads", reads())
	}
	clk.add(time.Minute)
	read()
	if reads() != 2 {
		t.Fatalf("near the lease's end, read again: %d reads", reads())
	}
	// A lease that's already ending isn't kept at all.
	clk.add(3 * time.Minute)
	st.mu.Lock()
	st.leaseEnd = clk.now().Add(30 * time.Second)
	st.mu.Unlock()
	read()
	read()
	if reads() != 4 {
		t.Fatalf("an ending lease isn't kept: %d reads", reads())
	}
}

// signV4 gives AWS's own example signature (IAM ListUsers, in AWS's Signature Version 4 docs).
func TestSignV4MatchesAWSExample(t *testing.T) {
	req, _ := http.NewRequest(http.MethodGet, "https://iam.amazonaws.com/?Action=ListUsers&Version=2010-05-08", nil)
	req.Header.Set("Content-Type", "application/x-www-form-urlencoded; charset=utf-8")
	signV4(req, nil, awsCredentials{id: "AKIDEXAMPLE", secret: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY"}, "us-east-1", "iam", time.Date(2015, 8, 30, 12, 36, 0, 0, time.UTC))
	want := "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/iam/aws4_request, SignedHeaders=content-type;host;x-amz-date, Signature=5d672d79c15b13162d9279b0855cfba6789a8edb4c82c400e06b5924a6f2b5d7"
	if got := req.Header.Get("Authorization"); got != want {
		t.Fatalf("got  %s\nwant %s", got, want)
	}
}
