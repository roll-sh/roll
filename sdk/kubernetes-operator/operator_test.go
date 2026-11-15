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
	// Workloads restarted (spec.rolloutRestart): their paths.
	restarted []string
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
	case strings.HasPrefix(p, "/apis/apps/v1/namespaces/") && r.Method == http.MethodPatch:
		var b map[string]any
		_ = json.NewDecoder(r.Body).Decode(&b)
		ann := b["spec"].(map[string]any)["template"].(map[string]any)["metadata"].(map[string]any)["annotations"].(map[string]any)
		if ann["roll.sh/restartedAt"] == nil {
			write(400, map[string]string{"message": "no restart annotation"})
			return
		}
		f.restarted = append(f.restarted, p)
		write(200, nil)
	case strings.HasSuffix(p, "/token") && strings.Contains(p, "/serviceaccounts/") && r.Method == http.MethodPost:
		// A TokenRequest: the token names the service account it's for and its audience.
		var b struct {
			Spec struct {
				Audiences []string `json:"audiences"`
			} `json:"spec"`
		}
		_ = json.NewDecoder(r.Body).Decode(&b)
		parts := strings.Split(strings.TrimPrefix(p, "/api/v1/namespaces/"), "/") // {ns}/serviceaccounts/{sa}/token
		write(200, map[string]any{"status": map[string]string{"token": "sa:" + parts[0] + ":" + parts[2] + ":" + strings.Join(b.Spec.Audiences, ",")}})
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
		// The cluster's identity takes the operator's own token; team-a's only team-a's app account.
		ok := b["identity"] == "idn_cluster" && b["token"] == "projected-sa-token" || b["identity"] == "idn_team_a" && strings.HasPrefix(b["token"], "sa:team-a:app:http")
		if !ok {
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
		if n.name == "web" {
			rs.Spec.RolloutRestart = []restartTarget{{Kind: "Deployment", Name: "web"}}
		}
		kubeAPI.rs = append(kubeAPI.rs, rs)
	}
	rolld := &fakeRolld{digest: "d1", value: "postgres://db/one"}
	k8s, roll := httptest.NewServer(kubeAPI), httptest.NewServer(rolld)
	defer k8s.Close()
	defer roll.Close()
	o := &operator{
		kube: &kube{api: k8s.URL, token: "sa", http: http.DefaultClient}, api: roll.URL, identity: "idn_cluster",
		oidc: func() (string, error) { return "projected-sa-token", nil }, now: time.Now, clients: map[string]*secrets.Client{},
		namespaces: map[string]bool{"apps": true},
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
	// What reads it restarted on the change, and not when the Secret was first made.
	if len(kubeAPI.restarted) != 1 || kubeAPI.restarted[0] != "/apis/apps/v1/namespaces/apps/deployments/web" {
		t.Fatalf("restarted: %v", kubeAPI.restarted)
	}
	// The digest moving with the same values (a key rewritten as it was) restarts nothing.
	rolld.mu.Lock()
	rolld.digest = "d3"
	rolld.mu.Unlock()
	if err := o.pass(ctx); err != nil || len(kubeAPI.restarted) != 1 {
		t.Fatalf("restarted on unchanged data: %v (%v)", kubeAPI.restarted, err)
	}
}

// Outside the operator's own namespaces a RollSecret logs in as a service account of its own
// namespace: creating one there can't borrow the operator's login, or another team's identity.
func TestARollSecretElsewhereLogsInAsItsOwnNamespacesServiceAccount(t *testing.T) {
	kubeAPI := &fakeKube{secrets: map[string]*secretObj{}, status: map[string]rollStatus{}}
	add := func(ns, name, sa, identity string) {
		var rs rollSecret
		rs.Metadata = meta{Name: name, Namespace: ns, UID: "uid-" + ns + "-" + name, Generation: 1}
		rs.Spec.Environment, rs.Spec.ServiceAccountName, rs.Spec.Identity = "production", sa, identity
		kubeAPI.rs = append(kubeAPI.rs, rs)
	}
	add("team-a", "web", "app", "idn_team_a")
	add("team-b", "borrowed", "", "")            // the operator's own login
	add("team-b", "stolen", "app", "idn_team_a") // team-a's identity, team-b's account
	rolld := &fakeRolld{digest: "d1", value: "postgres://db/one"}
	k8s, roll := httptest.NewServer(kubeAPI), httptest.NewServer(rolld)
	defer k8s.Close()
	defer roll.Close()
	o := &operator{
		kube: &kube{api: k8s.URL, token: "sa", http: http.DefaultClient}, api: roll.URL, identity: "idn_cluster",
		oidc: func() (string, error) { return "projected-sa-token", nil }, now: time.Now, clients: map[string]*secrets.Client{},
		namespaces: map[string]bool{"roll-system": true},
	}
	if err := o.pass(context.Background()); err != nil {
		t.Fatal(err)
	}
	if s := kubeAPI.secrets["team-a/web"]; s == nil || string(s.Data["DB_URL"]) != "postgres://db/one" {
		t.Fatalf("team-a's own account: %+v %+v", s, kubeAPI.status["team-a/web"])
	}
	if st := kubeAPI.status["team-b/borrowed"]; !strings.Contains(st.Error, "set spec.serviceAccountName") {
		t.Fatalf("the operator's login borrowed: %+v", st)
	}
	if st := kubeAPI.status["team-b/stolen"]; !strings.Contains(st.Error, "no identity takes this token") {
		t.Fatalf("another team's identity: %+v", st)
	}
	if len(kubeAPI.secrets) != 1 {
		t.Fatalf("Secrets written: %d", len(kubeAPI.secrets))
	}
}
