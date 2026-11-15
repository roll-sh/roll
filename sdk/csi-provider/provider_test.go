package main

import (
	"bytes"
	"context"
	"encoding/binary"
	"encoding/json"
	"errors"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"net/url"
	"path/filepath"
	"strings"
	"sync"
	"testing"
)

// rollState is what fakeRoll counts and takes: its logins, and the pods' current roll token.
type rollState struct {
	mu     sync.Mutex
	logins int
	pod    string
}

// fakeRoll answers a workload identity's login and the values route as rolld does.
func fakeRoll(t *testing.T) *httptest.Server {
	srv, _ := fakeRollState(t)
	return srv
}

func fakeRollState(t *testing.T) (*httptest.Server, *rollState) {
	st := &rollState{pod: "rlst_pod"}
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		st.mu.Lock()
		defer st.mu.Unlock()
		answer := func(status int, v any) {
			w.WriteHeader(status)
			_ = json.NewEncoder(w).Encode(v)
		}
		switch {
		case r.Method == http.MethodPost && r.URL.Path == "/v1/secrets/login":
			var b struct{ Identity, Token string }
			_ = json.NewDecoder(r.Body).Decode(&b)
			if b.Identity != "idn_1" || b.Token != "sa-token" {
				answer(401, map[string]string{"error": "the token isn't the identity's"})
				return
			}
			st.logins++
			answer(200, map[string]string{"token": st.pod, "project": "prj_1"})
		case r.URL.Path == "/v1/secrets/projects/prj_1/environments/production/values":
			if a := r.Header.Get("Authorization"); a != "Bearer "+st.pod && a != "Bearer rlt_node" {
				answer(401, map[string]string{"error": "unknown token"})
				return
			}
			q := r.URL.Query()
			if q.Get("strict") != "1" || q.Get("digest") != "1" {
				t.Errorf("a mount reads strictly, with the digest: %s", r.URL.RawQuery)
			}
			if q.Get("path") == "/broken" {
				answer(409, map[string]string{"error": "1 didn't resolve: BROKEN (strict: nothing is answered)", "code": "unresolved"})
				return
			}
			values := map[string]string{"DATABASE_URL": "postgres://app:s3cret@db:5432/shop", "NAME": `it's "q" $x`}
			dynamic := []string{"TEMP_KEY"}
			if q.Get("dynamic") == "true" {
				values["TEMP_KEY"] = "AKIA-temp"
				dynamic = nil
			}
			answer(200, map[string]any{"values": values, "dynamic": dynamic, "digest": "d1"})
		default:
			answer(404, map[string]string{"error": "no route"})
		}
	}))
	t.Cleanup(srv.Close)
	return srv, st
}

// invoke is a gRPC call as the driver makes it: HTTP/2 with prior knowledge on the socket, one
// framed message each way, the status in the trailers.
type invoke func(ctx context.Context, method string, req interface{ marshal() []byte }, out interface{ unmarshal([]byte) error }) error

// driver is the CSI driver's side: a client on the provider's socket.
func driver(t *testing.T) (invoke, func(ctx context.Context, req *mountRequest) (*mountResponse, error)) {
	sock := filepath.Join(t.TempDir(), "roll.sock")
	l, err := net.Listen("unix", sock)
	if err != nil {
		t.Fatal(err)
	}
	srv := serve(l, &provider{http: http.DefaultClient, version: "test", allowHTTP: true})
	t.Cleanup(func() { srv.Close() })
	protocols := new(http.Protocols)
	protocols.SetUnencryptedHTTP2(true)
	client := &http.Client{Transport: &http.Transport{
		Protocols: protocols,
		DialContext: func(ctx context.Context, _, _ string) (net.Conn, error) {
			return (&net.Dialer{}).DialContext(ctx, "unix", sock)
		},
	}}
	call := func(ctx context.Context, method string, req interface{ marshal() []byte }, out interface{ unmarshal([]byte) error }) error {
		msg := req.marshal()
		body := binary.BigEndian.AppendUint32([]byte{0}, uint32(len(msg)))
		r, _ := http.NewRequestWithContext(ctx, http.MethodPost, "http://localhost/v1alpha1.CSIDriverProvider/"+method, bytes.NewReader(append(body, msg...)))
		r.Header.Set("Content-Type", "application/grpc")
		r.Header.Set("TE", "trailers")
		res, err := client.Do(r)
		if err != nil {
			return err
		}
		defer res.Body.Close()
		data, err := io.ReadAll(res.Body)
		if err != nil {
			return err
		}
		if code := res.Trailer.Get("Grpc-Status"); code != "0" {
			m, _ := url.PathUnescape(res.Trailer.Get("Grpc-Message"))
			return errors.New("status " + code + ": " + m)
		}
		if res.ProtoMajor != 2 || len(data) < 5 || int(binary.BigEndian.Uint32(data[1:5])) != len(data)-5 {
			t.Fatalf("not a gRPC answer: HTTP/%d, %d bytes", res.ProtoMajor, len(data))
		}
		return out.unmarshal(data[5:])
	}
	mount := func(ctx context.Context, req *mountRequest) (*mountResponse, error) {
		out := &mountResponse{}
		return out, call(ctx, "Mount", req, out)
	}
	return call, mount
}

func attributes(api string, extra map[string]string) string {
	a := map[string]string{
		"api":                         api,
		"identity":                    "idn_1",
		"environment":                 "production",
		"csi.storage.k8s.io/pod.name": "web-0",
	}
	// The driver passes the pod's token for each audience in its tokenRequests: here the API's URL.
	tokens, _ := json.Marshal(map[string]any{api: map[string]string{"token": "sa-token", "expirationTimestamp": "2026-11-14T20:00:00Z"}})
	a["csi.storage.k8s.io/serviceAccount.tokens"] = string(tokens)
	for k, v := range extra {
		a[k] = v
	}
	b, _ := json.Marshal(a)
	return string(b)
}

// A pod's volume gets the environment's values as files (each key's own, a dotenv file or JSON),
// logged in with its own service account token, versioned by the folder's digest.
func TestAMountGetsTheEnvironmentAsFiles(t *testing.T) {
	roll, st := fakeRollState(t)
	call, mount := driver(t)
	ctx := context.Background()
	v := &versionResponse{}
	if err := call(ctx, "Version", &versionRequest{Version: "v1alpha1"}, v); err != nil || v.RuntimeName != "roll" {
		t.Fatalf("version: %+v %v", v, err)
	}

	r, err := mount(ctx, &mountRequest{Attributes: attributes(roll.URL, nil), Permission: "420", TargetPath: "/var/lib/kubelet/pods/x/volumes/y"})
	if err != nil {
		t.Fatal(err)
	}
	got := map[string]string{}
	for _, f := range r.Files {
		got[f.Path] = string(f.Contents)
		if f.Mode != 420 {
			t.Errorf("%s: mode %o", f.Path, f.Mode)
		}
	}
	if len(got) != 2 || got["DATABASE_URL"] != "postgres://app:s3cret@db:5432/shop" || got["NAME"] != `it's "q" $x` {
		t.Fatalf("files: %v", got)
	}
	if len(r.ObjectVersion) != 2 || r.ObjectVersion[0].Version != "d1" {
		t.Errorf("object versions: %+v", r.ObjectVersion)
	}

	// One dotenv file, quoted as roll's own export quotes it.
	r, err = mount(ctx, &mountRequest{Attributes: attributes(roll.URL, map[string]string{"format": "dotenv"})})
	if err != nil {
		t.Fatal(err)
	}
	want := "DATABASE_URL=postgres://app:s3cret@db:5432/shop\nNAME=\"it's \\\"q\\\" \\$x\"\n"
	if len(r.Files) != 1 || r.Files[0].Path != ".env" || string(r.Files[0].Contents) != want {
		t.Fatalf("dotenv: %+v", r.Files)
	}

	// Chosen keys, renamed; a dynamic credential only when asked for.
	r, err = mount(ctx, &mountRequest{Attributes: attributes(roll.URL, map[string]string{"keys": "DATABASE_URL=db-url\nTEMP_KEY", "dynamic": "true"})})
	if err != nil || len(r.Files) != 2 || r.Files[0].Path != "db-url" || string(r.Files[1].Contents) != "AKIA-temp" {
		t.Fatalf("keys: %+v %v", r, err)
	}

	// A node's roll token from nodePublishSecretRef, where pods have no identity.
	r, err = mount(ctx, &mountRequest{Attributes: attributes(roll.URL, map[string]string{"identity": "", "project": "prj_1"}), Secrets: `{"token":"rlt_node"}`})
	if err != nil || len(r.Files) != 2 {
		t.Fatalf("node token: %+v %v", r, err)
	}

	// The pod logged in once for its three mounts (a rotation poll, a sibling with the same
	// token); once its roll token is refused, the next mount logs in again and still works.
	st.mu.Lock()
	logins := st.logins
	st.pod = "rlst_pod2"
	st.mu.Unlock()
	if logins != 1 {
		t.Fatalf("%d logins for one service account token", logins)
	}
	if r, err = mount(ctx, &mountRequest{Attributes: attributes(roll.URL, nil)}); err != nil || len(r.Files) != 2 || st.logins != 2 {
		t.Fatalf("after the token was refused: %+v %v (%d logins)", r, err, st.logins)
	}
}

// A mount that can't get everything it asked for fails (the pod waits, a rotation keeps what it
// had) and says why, naming keys and never a value.
func TestAMountWithoutAWholeAnswerFails(t *testing.T) {
	roll := fakeRoll(t)
	_, mount := driver(t)
	ctx := context.Background()
	for _, c := range []struct {
		name  string
		extra map[string]string
		want  string
	}{
		{"unresolved", map[string]string{"path": "/broken"}, "BROKEN (strict"},
		{"dynamic", map[string]string{"keys": "TEMP_KEY"}, `TEMP_KEY is a dynamic credential: set parameters.dynamic: "true"`},
		{"unset", map[string]string{"keys": "NOPE"}, "NOPE isn't set there"},
		{"audience", map[string]string{"audience": "vault"}, `no service account token for audience "vault"`},
		{"refused", map[string]string{"identity": "idn_2"}, "the token isn't the identity's"},
		{"no login", map[string]string{"identity": ""}, "set parameters.identity"},
		{"file", map[string]string{"keys": "DATABASE_URL=../etc/passwd"}, "isn't a file name"},
	} {
		_, err := mount(ctx, &mountRequest{Attributes: attributes(roll.URL, c.extra)})
		if err == nil || !strings.Contains(err.Error(), c.want) {
			t.Errorf("%s: %v", c.name, err)
		}
		if err != nil && strings.Contains(err.Error(), "s3cret") {
			t.Errorf("%s: a value in an error: %v", c.name, err)
		}
	}
}

// A pod's token goes only over TLS, and only to the APIs the provider's operator allows.
func TestTheAPIIsHTTPSAndOneTheProviderServes(t *testing.T) {
	p := &provider{http: http.DefaultClient, apis: []string{"https://api.roll.example"}}
	if _, err := p.parse(map[string]string{"api": "http://api.roll.example", "environment": "production"}); err == nil || !strings.Contains(err.Error(), "https://") {
		t.Fatalf("plain http: %v", err)
	}
	if _, err := p.parse(map[string]string{"api": "https://elsewhere.example", "environment": "production"}); err == nil || !strings.Contains(err.Error(), "isn't one this provider serves") {
		t.Fatalf("another API: %v", err)
	}
	if _, err := p.parse(map[string]string{"api": "https://api.roll.example/", "environment": "production"}); err != nil {
		t.Fatalf("its own API: %v", err)
	}
}
