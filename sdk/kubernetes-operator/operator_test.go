package main

import (
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/roll-sh/roll/sdk/go/secrets"
)

// A cluster as the operator sees it: two RollSecrets, and a Secret someone else made.
type fakeKube struct {
	mu      sync.Mutex
	rs      []rollSecret
	secrets map[string]*secretObj // ns/name
	writes  int
	status  map[string]rollStatus // ns/name
}

func (f *fakeKube) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	f.mu.Lock()
	defer f.mu.Unlock()
	write := func(code int, v any) {
		w.WriteHeader(code)
		if v != nil {
			_ = json.NewEncoder(w).Encode(v)
		}
	}
	p := r.URL.Path
	switch {
	case p == "/apis/roll.sh/v1/rollsecrets":
		for i := range f.rs {
			f.rs[i].Status = f.status[f.rs[i].Metadata.Namespace+"/"+f.rs[i].Metadata.Name]
		}
		write(200, map[string]any{"items": f.rs})
	case strings.HasSuffix(p, "/status") && r.Method == http.MethodPatch:
		var b struct {
			Status rollStatus `json:"status"`
		}
		_ = json.NewDecoder(r.Body).Decode(&b)
		parts := strings.Split(p, "/") // /apis/roll.sh/v1/namespaces/{ns}/rollsecrets/{name}/status
		f.status[parts[5]+"/"+parts[7]] = b.Status
		write(200, nil)
	case strings.HasPrefix(p, "/api/v1/namespaces/"):
		parts := strings.Split(strings.TrimPrefix(p, "/api/v1/namespaces/"), "/") // {ns}/secrets[/{name}]
		ns := parts[0]
		switch r.Method {
		case http.MethodGet:
			s := f.secrets[ns+"/"+parts[2]]
			if s == nil {
				write(404, map[string]string{"message": "not found"})
				return
			}
			write(200, s)
		case http.MethodPost, http.MethodPut:
			var s secretObj
			_ = json.NewDecoder(r.Body).Decode(&s)
			f.writes++
			s.Metadata.ResourceVersion = "rv" + strings.Repeat("1", f.writes)
			f.secrets[ns+"/"+s.Metadata.Name] = &s
			write(200, s)
		}
	default:
		write(404, map[string]string{"message": "no route " + p})
	}
}

// rolld for a workload identity: its login's token ends when rolld says so.
type fakeRolld struct {
	mu     sync.Mutex
	logins int
	live   string // the token that works
	digest string
	value  string
}

func (f *fakeRolld) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	f.mu.Lock()
	defer f.mu.Unlock()
	write := func(code int, v any) {
		w.WriteHeader(code)
		_ = json.NewEncoder(w).Encode(v)
	}
	if r.URL.Path == "/v1/secrets/login" {
		var b map[string]string
		_ = json.NewDecoder(r.Body).Decode(&b)
		if b["identity"] != "idn_cluster" || b["token"] != "projected-sa-token" {
			write(401, map[string]string{"error": "no identity takes this token"})
			return
		}
		f.logins++
		f.live = "rlt_login" + strings.Repeat("x", f.logins)
		write(200, map[string]string{"token": f.live, "project": "prj_a"})
		return
	}
	if r.Header.Get("Authorization") != "Bearer "+f.live {
		write(401, map[string]string{"error": "this token ended"})
		return
	}
	switch r.URL.Path {
	case "/v1/secrets/projects/prj_a/environments/production/digest":
		write(200, map[string]string{"digest": f.digest})
	case "/v1/secrets/projects/prj_a/environments/production/values":
		write(200, map[string]any{"values": map[string]string{"DB_URL": f.value}, "problems": []any{}})
	default:
		write(404, map[string]string{"error": "no route"})
	}
}

func TestTheOperatorKeepsItsSecretsInStepAndTouchesNoOtherSecret(t *testing.T) {
	kubeAPI := &fakeKube{secrets: map[string]*secretObj{}, status: map[string]rollStatus{}}
	kubeAPI.secrets["apps/taken"] = &secretObj{Metadata: meta{Name: "taken", Namespace: "apps"}, Data: map[string][]byte{"MINE": []byte("keep")}}
	for _, n := range []struct{ name, secret string }{{"web", ""}, {"clash", "taken"}} {
		var rs rollSecret
		rs.Metadata = meta{Name: n.name, Namespace: "apps", UID: "uid-" + n.name, Generation: 1}
		rs.Spec.Environment, rs.Spec.SecretName = "production", n.secret
		kubeAPI.rs = append(kubeAPI.rs, rs)
	}
	rolld := &fakeRolld{digest: "d1", value: "postgres://db/one"}
	k8s, roll := httptest.NewServer(kubeAPI), httptest.NewServer(rolld)
	defer k8s.Close()
	defer roll.Close()
	o := &operator{
		kube: &kube{api: k8s.URL, token: "sa", http: http.DefaultClient}, api: roll.URL, identity: "idn_cluster",
		oidc: func() (string, error) { return "projected-sa-token", nil }, now: time.Now, clients: map[string]*secrets.Client{},
	}
	ctx := context.Background()

	if err := o.pass(ctx); err != nil {
		t.Fatal(err)
	}
	web := kubeAPI.secrets["apps/web"]
	if web == nil || string(web.Data["DB_URL"]) != "postgres://db/one" || web.Metadata.OwnerReferences[0].UID != "uid-web" || web.Metadata.Labels[managedLabel] != managedValue {
		t.Fatalf("the Secret: %+v", web)
	}
	if st := kubeAPI.status["apps/web"]; st.Digest != "d1" || st.Keys != 1 || st.Error != "" {
		t.Fatalf("its status: %+v", st)
	}
	if st := kubeAPI.status["apps/clash"]; !strings.Contains(st.Error, "isn't this RollSecret's") || string(kubeAPI.secrets["apps/taken"].Data["MINE"]) != "keep" {
		t.Fatalf("someone else's Secret: %+v %+v", st, kubeAPI.secrets["apps/taken"])
	}

	// Nothing changed: nothing written.
	writes := kubeAPI.writes
	if err := o.pass(ctx); err != nil || kubeAPI.writes != writes {
		t.Fatalf("an unchanged environment wrote %d times (%v)", kubeAPI.writes-writes, err)
	}

	// The environment changed, and the login's token ended meanwhile: logged in again, updated.
	rolld.mu.Lock()
	rolld.digest, rolld.value, rolld.live = "d2", "postgres://db/two", "ended"
	rolld.mu.Unlock()
	if err := o.pass(ctx); err != nil {
		t.Fatal(err)
	}
	if got := string(kubeAPI.secrets["apps/web"].Data["DB_URL"]); got != "postgres://db/two" || rolld.logins != 2 {
		t.Fatalf("after the change: %q, %d logins", got, rolld.logins)
	}
}
