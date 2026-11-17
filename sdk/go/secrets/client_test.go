package secrets

import (
	"context"
	"crypto/ecdh"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"sync"
	"testing"
	"time"
)

// A box sealed by @roll/secrets' sealTo (the same box as the server's seal_to), with a fixed key.
const (
	vectorKey     = "AQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyA"
	vectorContext = "roll-secrets:prj_a:production:app:web"
)

var vectorBox = Sealed{V: 1, EPK: "aAGjVaESpknkSH_uG48K1a_unnp97Gh4uRwOYWrjHkk", Nonce: "yct4_uAUdnUKxskR", CT: "-UbR7dzvRmoXgBHnbyoW00qyGY07tIB_0vx6VrXQhPPRNns7PU3irIJq5M5Xjto"}

func vectorPrivateKey(t *testing.T) *ecdh.PrivateKey {
	raw, err := b64.DecodeString(vectorKey)
	if err != nil {
		t.Fatal(err)
	}
	k, err := ecdh.X25519().NewPrivateKey(raw)
	if err != nil {
		t.Fatal(err)
	}
	return k
}

func TestABoxSealedByRollOpensOnlyForItsContext(t *testing.T) {
	k := vectorPrivateKey(t)
	plain, err := Open(vectorBox, k, vectorContext)
	if err != nil || string(plain) != `{"DB_URL":"postgres://db/shop"}` {
		t.Fatalf("%q %v", plain, err)
	}
	if _, err := Open(vectorBox, k, "roll-secrets:prj_a:staging:app:web"); err == nil {
		t.Fatal("opened for another purpose")
	}
	other, _ := GenerateKey()
	if _, err := Open(vectorBox, other, vectorContext); err == nil {
		t.Fatal("opened with another key")
	}
}

// rolld as the client sees it: a workload identity's login, whoami, values, a sealed resolve, and a
// digest that moves when the values change.
func TestAWorkloadLogsInReadsSealedValuesAndWatchesForChanges(t *testing.T) {
	var mu sync.Mutex
	digest, value, current, logins := "d1", "one", "rlt_workload", 0
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		mu.Lock()
		defer mu.Unlock()
		auth := r.Header.Get("Authorization")
		write := func(v any) { _ = json.NewEncoder(w).Encode(v) }
		switch {
		case r.URL.Path == "/v1/secrets/login":
			var b map[string]string
			_ = json.NewDecoder(r.Body).Decode(&b)
			if b["identity"] != "idn_ci" || b["token"] != "the-oidc-token" {
				w.WriteHeader(http.StatusUnauthorized)
				write(map[string]string{"error": "no identity idn_ci takes this token", "code": "unauthorized"})
				return
			}
			logins++
			write(map[string]any{"token": current, "project": "prj_a", "expires_at": time.Now().Add(time.Hour).UnixMilli()})
		case auth != "Bearer "+current:
			w.WriteHeader(http.StatusUnauthorized)
		case r.URL.Path == "/v1/secrets/projects/prj_a/environments/production/digest":
			write(map[string]string{"digest": digest})
		case r.URL.Path == "/v1/secrets/projects/prj_a/environments/production/values":
			write(map[string]any{"values": map[string]string{"A": value}, "problems": []any{}, "leases": []string{}})
		case r.URL.Path == "/v1/secrets/projects/prj_a/environments/production/resolve":
			var b map[string]string
			_ = json.NewDecoder(r.Body).Decode(&b)
			if b["consumer"] != "app:web" || b["seal_to"] != PublicKey(vectorPrivateKey(t)) {
				w.WriteHeader(http.StatusBadRequest)
				return
			}
			write(map[string]any{"sealed": vectorBox, "context": vectorContext, "problems": []any{}, "leases": []string{}, "dynamic": []string{}, "linked": []string{"DB_URL"}})
		default:
			w.WriteHeader(http.StatusNotFound)
		}
	}))
	defer srv.Close()
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()

	t.Setenv("ROLL_API_URL", srv.URL)
	t.Setenv("ROLL_IDENTITY", "idn_ci")
	t.Setenv("ROLL_OIDC_TOKEN", "wrong")
	if _, err := FromEnv(ctx); err == nil || err.(*Error).Status != http.StatusUnauthorized {
		t.Fatalf("a token no identity takes: %v", err)
	}
	t.Setenv("ROLL_OIDC_TOKEN", "the-oidc-token")
	c, err := FromEnv(ctx)
	if err != nil {
		t.Fatal(err)
	}

	sealed, err := c.ResolveSealed(ctx, "production", "", "app:web", vectorPrivateKey(t))
	if err != nil || sealed.Values["DB_URL"] != "postgres://db/shop" || len(sealed.Linked) != 1 {
		t.Fatalf("%+v %v", sealed, err)
	}

	seen := make(chan string, 4)
	go func() {
		_ = c.Watch(ctx, "production", Watch{Every: 10 * time.Millisecond, OnChange: func(v *Values) error {
			seen <- v.Values["A"]
			return nil
		}})
	}()
	if got := <-seen; got != "one" {
		t.Fatalf("first %q", got)
	}
	mu.Lock()
	digest, value = "d2", "two"
	mu.Unlock()
	if got := <-seen; got != "two" {
		t.Fatalf("after the change %q", got)
	}
	// The roll token stops working (revoked, ended): the client logs in again and keeps watching.
	mu.Lock()
	digest, value, current = "d3", "three", "rlt_workload2"
	mu.Unlock()
	if got := <-seen; got != "three" {
		t.Fatalf("after a new login %q", got)
	}
	mu.Lock()
	if logins != 2 {
		t.Fatalf("%d logins, not 2", logins)
	}
	mu.Unlock()
	select {
	case got := <-seen:
		t.Fatalf("an unchanged digest delivered %q again", got)
	case <-time.After(50 * time.Millisecond):
	}
}

// Values are kept for CacheTTL, revalidated with their digest (a 304 keeps them), and never kept
// when they hold dynamic credentials.
func TestValuesAreCachedAndRevalidatedWithTheirDigest(t *testing.T) {
	var mu sync.Mutex
	digest, dynamic, value := "d1", []string{}, "1"
	var asked []string
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		mu.Lock()
		defer mu.Unlock()
		tag := r.Header.Get("If-None-Match")
		asked = append(asked, tag)
		if r.URL.Query().Get("digest") != "1" {
			t.Errorf("no digest asked for: %s", r.URL.RawQuery)
		}
		w.Header().Set("ETag", `"`+digest+`"`)
		if tag == `"`+digest+`"` {
			w.WriteHeader(http.StatusNotModified)
			return
		}
		_ = json.NewEncoder(w).Encode(map[string]any{"values": map[string]string{"A": value}, "problems": []any{}, "leases": []string{}, "dynamic": dynamic, "linked": []string{}})
	}))
	defer srv.Close()
	clock := time.Unix(0, 0)
	c := &Client{API: srv.URL, Token: "rlt_x", Project: "prj_1", CacheTTL: 20 * time.Second, Now: func() time.Time { return clock }}
	ctx := context.Background()
	get := func() string {
		v, err := c.Values(ctx, "production", "")
		if err != nil {
			t.Fatal(err)
		}
		return v.Values["A"]
	}
	if get() != "1" || get() != "1" || len(asked) != 1 {
		t.Fatalf("within the TTL, from memory: %q", asked)
	}
	clock = clock.Add(25 * time.Second)
	if get() != "1" || len(asked) != 2 || asked[1] != `"d1"` {
		t.Fatalf("revalidated: %q", asked)
	}
	clock = clock.Add(25 * time.Second)
	mu.Lock()
	digest, dynamic, value = "d2", []string{"PW"}, "2"
	mu.Unlock()
	if get() != "2" || get() != "2" || len(asked) != 4 {
		t.Fatalf("a change, then dynamic credentials read every time: %q", asked)
	}
	// CacheSize: past it, the folder read longest ago goes.
	mu.Lock()
	dynamic = []string{}
	mu.Unlock()
	c = &Client{API: srv.URL, Token: "rlt_x", Project: "prj_1", CacheTTL: 20 * time.Second, CacheSize: 1, Now: func() time.Time { return clock }}
	for _, env := range []string{"production", "production", "staging", "production"} {
		if _, err := c.Values(ctx, env, ""); err != nil {
			t.Fatal(err)
		}
	}
	if len(asked) != 7 {
		t.Fatalf("one folder kept: %q", asked)
	}
}

// Transit sends bytes as base64 and decrypts back to the same bytes, as @roll/secrets' does.
func TestTransitSendsBase64AndDecryptsBackToTheSameBytes(t *testing.T) {
	var sent []map[string]any
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		var b map[string]any
		_ = json.NewDecoder(r.Body).Decode(&b)
		b["path"] = r.URL.Path
		sent = append(sent, b)
		// A server that "encrypts" by echoing: enough to check the encoding both ways.
		if r.URL.Path == "/v1/secrets/projects/prj_1/transit/cards/encrypt" {
			_ = json.NewEncoder(w).Encode(map[string]string{"ciphertext": "roll:v1:" + b["plaintext"].(string)})
			return
		}
		_ = json.NewEncoder(w).Encode(map[string]string{"plaintext": b["ciphertext"].(string)[len("roll:v1:"):]})
	}))
	defer srv.Close()
	cards := (&Client{API: srv.URL, Token: "rlt_x", Project: "prj_1"}).Transit("cards")
	ctx := context.Background()
	ct, err := cards.Encrypt(ctx, []byte("héllo ✓"), "order:42")
	if err != nil || sent[0]["plaintext"] != "aMOpbGxvIOKckw==" || sent[0]["context"] != "order:42" {
		t.Fatalf("sent %v (%v)", sent, err)
	}
	if got, err := cards.Decrypt(ctx, ct, "order:42"); err != nil || string(got) != "héllo ✓" {
		t.Fatalf("decrypted %q (%v)", got, err)
	}
	raw := []byte{0, 255, 128, 7}
	ct, _ = cards.Encrypt(ctx, raw, "")
	if got, _ := cards.Decrypt(ctx, ct, ""); string(got) != string(raw) || sent[2]["context"] != nil {
		t.Fatalf("bytes: %v, context %v", got, sent[2]["context"])
	}
}
