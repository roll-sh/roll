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
	digest, value := "d1", "one"
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
			write(map[string]any{"token": "rlt_workload", "project": "prj_a", "expires_at": 1})
		case auth != "Bearer rlt_workload":
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
	select {
	case got := <-seen:
		t.Fatalf("an unchanged digest delivered %q again", got)
	case <-time.After(50 * time.Millisecond):
	}
}
