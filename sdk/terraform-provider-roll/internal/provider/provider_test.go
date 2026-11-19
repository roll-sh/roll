package provider

import (
	"encoding/json"
	"fmt"
	"net/http"
	"net/http/httptest"
	"os"
	"regexp"
	"sort"
	"strings"
	"sync"
	"testing"

	"github.com/hashicorp/terraform-plugin-framework/providerserver"
	"github.com/hashicorp/terraform-plugin-go/tfprotov6"
	"github.com/hashicorp/terraform-plugin-testing/helper/resource"
	"github.com/hashicorp/terraform-plugin-testing/terraform"
	"github.com/hashicorp/terraform-plugin-testing/tfversion"
)

// rolld's secrets API in memory: environments, keys (a value or a link) and values, for one
// project, with the operator's token.
type fakeRolld struct {
	mu         sync.Mutex
	envs       map[string]*environment
	keys       map[string]map[string]any // env:path/key → {value|link, comment, version}
	identities map[string]map[string]any
	syncs      map[string]map[string]any
	tokens     map[string]map[string]any // roll-access's grants, kept when revoked
	// Machine clients by id: {grant (an id in tokens), oidc | public_jwk}; and each rotate's body.
	clients   map[string]map[string]any
	rotations []map[string]any
	// Dynamic credentials' leases a read with dynamic=true issued, and those revoked since.
	issued, revoked []string
	// What a project registers itself: cas, ssh-cas, transit, outside-databases.
	regs map[string]map[string]map[string]any
	// The admin URLs databases were registered with (never listed back).
	adminURLs []string
	// Credentials roll was sent and never lists back: private keys, SIEM tokens, rotator URLs.
	received []string
	// The project's required keys, and the KMS key wrapping its data key ("": none).
	required []any
	kms      string
}

// settings serves …/required and …/kms: one value per project.
func (f *fakeRolld) settings(r *http.Request, write func(int, any)) {
	var b any
	_ = json.NewDecoder(r.Body).Decode(&b)
	if strings.HasSuffix(r.URL.Path, "/required") {
		if r.Method == http.MethodPut {
			// As roll keeps them: the folder without a trailing /, sorted by folder and key.
			list := b.([]any)
			for _, x := range list {
				k := x.(map[string]any)
				if k["comment"] == nil {
					k["comment"] = ""
				}
				if p, _ := k["path"].(string); p == "" {
					k["path"] = "/"
				} else if p != "/" {
					k["path"] = strings.TrimSuffix(p, "/")
				}
			}
			sort.Slice(list, func(i, j int) bool {
				a, b := list[i].(map[string]any), list[j].(map[string]any)
				return a["path"].(string)+" "+a["key"].(string) < b["path"].(string)+" "+b["key"].(string)
			})
			f.required = list
		}
		write(200, append([]any{}, f.required...))
		return
	}
	switch r.Method {
	case http.MethodPut:
		f.kms = b.(map[string]any)["key"].(string)
	case http.MethodDelete:
		f.kms = ""
	}
	if f.kms == "" {
		write(200, map[string]any{"key": nil})
	} else {
		write(200, map[string]any{"key": f.kms})
	}
}

// machineClients serves /v1/access/clients: made (a key or CI's identity) with a grant in tokens,
// listed, rotated, revoked.
func (f *fakeRolld) machineClients(r *http.Request, api string, write func(int, any)) {
	rest := strings.TrimPrefix(strings.TrimPrefix(r.URL.Path, "/v1/access/clients"), "/")
	id, action, _ := strings.Cut(rest, "/")
	var b map[string]any
	_ = json.NewDecoder(r.Body).Decode(&b)
	credential := func(c map[string]any) {
		delete(c, "oidc")
		delete(c, "public_jwk")
		if o, ok := b["oidc"].(map[string]any); ok {
			if o["audience"] == nil {
				o["audience"] = api
			}
			c["oidc"] = o
		}
		if k := b["public_jwk"]; k != nil {
			c["public_jwk"] = k
		}
	}
	c := f.clients[id]
	switch {
	case r.Method == http.MethodPost && id == "":
		if (b["oidc"] == nil) == (b["public_jwk"] == nil) {
			write(400, map[string]string{"error": "one credential: public_jwk or oidc (Terraform never asks for a secret)"})
			return
		}
		grant := map[string]any{"id": fmt.Sprintf("tok_%d", len(f.tokens)+1), "name": b["name"], "rules": b["rules"], "expires_at": b["expires_at"], "ip_allow": []any{}, "revoked_at": nil}
		if b["ip_allow"] != nil {
			grant["ip_allow"] = b["ip_allow"]
		}
		f.tokens[grant["id"].(string)] = grant
		client := map[string]any{"grant": grant["id"]}
		credential(client)
		id = fmt.Sprintf("mcl_%d", len(f.clients)+1)
		f.clients[id] = client
		write(201, map[string]any{"client_id": id, "grant": grant, "token_endpoint": api + "/v1/access/oauth/token"})
	case r.Method == http.MethodGet && id == "":
		list := []any{}
		for id, c := range f.clients {
			var trusts any
			if o, ok := c["oidc"].(map[string]any); ok {
				trusts = map[string]any{"issuer": o["issuer"], "audience": o["audience"], "subject": o["subject"], "claims": o["claims"], "pinned_keys": nil}
			}
			list = append(list, map[string]any{"client_id": id, "signs_with_key": c["public_jwk"] != nil, "trusts": trusts, "last_sign_in": nil, "grant": f.tokens[c["grant"].(string)]})
		}
		write(200, list)
	case c == nil:
		write(404, map[string]string{"error": "machine client " + id})
	case r.Method == http.MethodPost && action == "rotate":
		f.rotations = append(f.rotations, b)
		credential(c)
		write(200, map[string]any{"grant": f.tokens[c["grant"].(string)]})
	case r.Method == http.MethodDelete && action == "":
		f.tokens[c["grant"].(string)]["revoked_at"] = 1
		write(200, map[string]any{"grant": f.tokens[c["grant"].(string)]})
	default:
		write(404, map[string]string{"error": "no route"})
	}
}

// grants serves roll-access's tokens: made with their rules, read, revoked.
func (f *fakeRolld) grants(r *http.Request, write func(int, any)) {
	id := strings.TrimPrefix(strings.TrimPrefix(r.URL.Path, "/v1/access/tokens"), "/")
	switch {
	case r.Method == http.MethodPost && id == "":
		var b map[string]any
		_ = json.NewDecoder(r.Body).Decode(&b)
		b["id"] = fmt.Sprintf("tok_%d", len(f.tokens)+1)
		b["revoked_at"] = nil
		f.tokens[b["id"].(string)] = b
		write(201, map[string]any{"grant": b, "token": "rlt_made_" + b["id"].(string)})
	case f.tokens[id] == nil:
		write(404, map[string]string{"error": "no token " + id})
	case r.Method == http.MethodDelete:
		f.tokens[id]["revoked_at"] = 1
		write(200, f.tokens[id])
	case r.Method == http.MethodPatch:
		var b map[string]any
		_ = json.NewDecoder(r.Body).Decode(&b)
		for k, v := range b {
			f.tokens[id][k] = v
		}
		write(200, f.tokens[id])
	default:
		write(200, f.tokens[id])
	}
}

// rest serves identities and syncs (create, list, delete), as roll keeps them.
func (f *fakeRolld) rest(w http.ResponseWriter, r *http.Request, kind string, items map[string]map[string]any, write func(int, any)) {
	id := strings.TrimPrefix(strings.TrimPrefix(r.URL.Path, "/v1/secrets/projects/prj_a/"+kind), "/")
	id, action, _ := strings.Cut(id, "/")
	host := func(u string) string { h, _, _ := strings.Cut(strings.TrimPrefix(u, "https://"), "/"); return h }
	switch {
	case r.Method == http.MethodPatch && items[id] != nil && action == "":
		var b map[string]any
		_ = json.NewDecoder(r.Body).Decode(&b)
		for k, v := range b {
			items[id][k] = v
		}
		if u, ok := b["url"].(string); ok {
			f.received = append(f.received, u)
			delete(items[id], "url")
			items[id]["host"] = host(u)
		}
		write(200, items[id])
	case r.Method == http.MethodPost && kind == "rotators" && items[id] != nil && action == "rotate-secret":
		answer := map[string]any{"secret": fmt.Sprintf("whsec_rotated_%d", len(f.received))}
		for k, v := range items[id] {
			answer[k] = v
		}
		write(200, answer)
	case r.Method == http.MethodPost && id == "":
		var b map[string]any
		_ = json.NewDecoder(r.Body).Decode(&b)
		b["id"] = fmt.Sprintf("%s_%d", kind[:3], len(items)+1)
		if kind == "identities" && b["ttl_minutes"] == nil {
			b["ttl_minutes"] = 15
		}
		switch kind {
		case "cas":
			b["certificate"], b["max_days"] = "-----BEGIN CERTIFICATE-----", 90
			// As roll keeps them: lowercase, without a trailing dot.
			var ds []any
			for _, d := range b["domains"].([]any) {
				ds = append(ds, strings.TrimSuffix(strings.ToLower(d.(string)), "."))
			}
			b["domains"] = ds
		case "ssh-cas":
			b["public_key"], b["max_hours"], b["max_host_days"] = "ssh-ed25519 AAAA roll-secrets:"+b["name"].(string), 24, 90
		case "transit":
			b["latest_version"] = 1
		case "rotators":
			f.received = append(f.received, b["url"].(string))
			b["host"] = host(b["url"].(string))
			delete(b, "url")
		case "audit/sinks":
			if t, ok := b["token"].(string); ok {
				f.received = append(f.received, t)
			}
			if b["format"] == nil {
				b["format"] = "roll"
			}
			delete(b, "token")
			delete(b, "from_start")
		case "github-apps":
			f.received = append(f.received, b["private_key"].(string))
			delete(b, "private_key")
		case "aws-roles":
			if b["trust"] == nil {
				b["trust"] = "external_id"
			}
		case "cloud-accounts":
			if b["cloud"] == "azure" && b["scope"] == nil {
				b["scope"] = "https://management.azure.com/.default"
			}
		case "outside-databases":
			f.adminURLs = append(f.adminURLs, b["url"].(string))
			delete(b, "url")
			b["host"] = "db.example.com"
			// Trimmed, as roll keeps it.
			if ca, ok := b["ca"].(string); ok {
				b["ca"] = strings.TrimSpace(ca)
			}
		}
		items[b["id"].(string)] = b
		answer := map[string]any{}
		for k, v := range b {
			answer[k] = v
		}
		if kind == "syncs" && b["kind"] == "webhook" {
			answer["signing_secret"] = "whsec_once"
		}
		if kind == "rotators" || kind == "audit/sinks" {
			answer["secret"] = "whsec_made_" + b["id"].(string)
		}
		write(201, answer)
	case r.Method == http.MethodGet && id == "":
		list := []map[string]any{}
		for _, v := range items {
			list = append(list, v)
		}
		if kind == "syncs" {
			write(200, map[string]any{"syncs": list, "kinds": []any{}})
		} else {
			write(200, list)
		}
	case r.Method == http.MethodDelete && items[id] != nil:
		delete(items, id)
		write(204, nil)
	case r.Method == http.MethodDelete && kind == "transit" && r.URL.Query().Get("confirm") == id:
		// Encryption keys go by name, with the name as confirmation.
		for k, v := range items {
			if v["name"] == id {
				delete(items, k)
				write(204, nil)
				return
			}
		}
		write(404, map[string]string{"error": "no key " + id})
	default:
		write(404, map[string]string{"error": "no " + kind + " " + id})
	}
}

func (f *fakeRolld) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	f.mu.Lock()
	defer f.mu.Unlock()
	write := func(code int, v any) {
		w.Header().Set("Content-Type", "application/json")
		w.WriteHeader(code)
		if v != nil {
			_ = json.NewEncoder(w).Encode(v)
		}
	}
	if r.Header.Get("Authorization") != "Bearer rlt_terraform" {
		write(401, map[string]string{"error": "no token"})
		return
	}
	if strings.HasPrefix(r.URL.Path, "/v1/access/tokens") {
		f.grants(r, write)
		return
	}
	api := "http://" + r.Host
	if strings.HasPrefix(r.URL.Path, "/v1/access/clients") {
		f.machineClients(r, api, write)
		return
	}
	if r.URL.Path == "/.well-known/oauth-authorization-server" {
		write(200, map[string]any{"issuer": api, "token_endpoint": api + "/v1/access/oauth/token"})
		return
	}
	if r.URL.Path == "/v1/secrets/projects/prj_a/required" || r.URL.Path == "/v1/secrets/projects/prj_a/kms" {
		f.settings(r, write)
		return
	}
	for _, kind := range []string{"identities", "syncs", "cas", "ssh-cas", "transit", "outside-databases", "rotators", "audit/sinks", "github-apps", "aws-roles", "aws-users", "cloud-accounts"} {
		if r.URL.Path == "/v1/secrets/projects/prj_a/"+kind || strings.HasPrefix(r.URL.Path, "/v1/secrets/projects/prj_a/"+kind+"/") {
			items := map[string]map[string]map[string]any{"identities": f.identities, "syncs": f.syncs}[kind]
			if items == nil {
				if f.regs == nil {
					f.regs = map[string]map[string]map[string]any{}
				}
				if f.regs[kind] == nil {
					f.regs[kind] = map[string]map[string]any{}
				}
				items = f.regs[kind]
			}
			f.rest(w, r, kind, items, write)
			return
		}
	}
	if lease, ok := strings.CutPrefix(r.URL.Path, "/v1/secrets/projects/prj_a/leases/"); ok && r.Method == http.MethodDelete {
		f.revoked = append(f.revoked, lease)
		write(204, nil)
		return
	}
	rest, ok := strings.CutPrefix(r.URL.Path, "/v1/secrets/projects/prj_a/environments/")
	if !ok {
		write(404, map[string]string{"error": "no route"})
		return
	}
	parts := strings.SplitN(rest, "/", 3)
	env := parts[0]
	var body map[string]any
	_ = json.NewDecoder(r.Body).Decode(&body)
	if len(parts) == 1 || parts[1] == "policy" {
		e := f.envs[env]
		switch {
		case r.Method == http.MethodPut && len(parts) == 1:
			if e == nil {
				f.envs[env] = &environment{Slug: env, Name: env}
			}
			write(200, map[string]any{"created": e == nil})
		case e == nil:
			write(404, map[string]string{"error": "no environment " + env})
		case r.Method == http.MethodPatch:
			e.Name = body["name"].(string)
			write(200, e)
		case r.Method == http.MethodPut:
			e.Protected = body["protected"].(bool)
			write(200, e)
		case r.Method == http.MethodDelete:
			if e.Protected && r.URL.Query().Get("confirm") != env {
				write(400, map[string]string{"error": "pass ?confirm"})
				return
			}
			delete(f.envs, env)
			write(204, nil)
		default:
			write(200, e)
		}
		return
	}
	if parts[1] == "values" {
		values := map[string]string{}
		for id, k := range f.keys {
			if e, key, _ := strings.Cut(id, ":/"); e == env {
				values[key] = k["value"].(string)
			}
		}
		leases := []string{}
		if r.URL.Query().Get("dynamic") == "true" {
			lease := fmt.Sprintf("lse_%d", len(f.issued)+1)
			f.issued = append(f.issued, lease)
			leases = append(leases, lease)
			values["TEMP_PASSWORD"] = "temporary"
		}
		write(200, map[string]any{"values": values, "problems": []any{}, "leases": leases})
		return
	}
	key, valueRoute := strings.CutSuffix(parts[2], "/value")
	id := env + ":" + r.URL.Query().Get("path") + key
	k := f.keys[id]
	switch {
	case r.Method == http.MethodPut:
		version := 1.0
		if k != nil {
			version = k["version"].(float64) + 1
		}
		body["version"] = version
		f.keys[id] = body
		write(200, map[string]any{"warnings": []string{}})
	case k == nil:
		write(404, map[string]string{"error": "no key " + key})
	case r.Method == http.MethodDelete:
		delete(f.keys, id)
		write(200, map[string]any{})
	case valueRoute:
		write(200, map[string]any{"value": k["value"]})
	default:
		write(200, map[string]any{"key": key, "path": "/", "version": k["version"], "comment": k["comment"], "link": k["link"], "source": map[string]string{"kind": "own"}})
	}
}

func TestEnvironmentsAndKeysAsCode(t *testing.T) {
	f := &fakeRolld{envs: map[string]*environment{}, keys: map[string]map[string]any{}, identities: map[string]map[string]any{}, syncs: map[string]map[string]any{}, tokens: map[string]map[string]any{}}
	srv := httptest.NewServer(f)
	defer srv.Close()
	config := fmt.Sprintf(`
provider "roll" {
  api_url = %q
  token   = "rlt_terraform"
  project = "prj_a"
}
resource "roll_secrets_environment" "production" {
  slug      = "production"
  name      = "Production"
  protected = true
}
resource "roll_secret" "db" {
  environment = roll_secrets_environment.production.slug
  key         = "DB_URL"
  value       = "postgres://db/shop"
  comment     = "the app's database"
}
data "roll_secrets_values" "production" {
  environment = roll_secret.db.environment
}
resource "roll_secrets_identity" "ci" {
  name         = "ci"
  issuer       = "https://token.actions.githubusercontent.com"
  audience     = %q
  subject      = "repo:acme/web:ref:refs/heads/main"
  environments = [roll_secrets_environment.production.slug]
  actions      = ["read", "reveal"]
}
resource "roll_secrets_token" "deploy" {
  name         = "deploy"
  environments = [roll_secrets_environment.production.slug]
  paths        = ["/api"]
  actions      = ["read", "reveal"]
  expires_at   = "2030-01-01T00:00:00Z"
}
resource "roll_secrets_sync" "deploys" {
  environment = roll_secrets_environment.production.slug
  kind        = "webhook"
  target      = { url = "https://hooks.example/roll" }
}
`, srv.URL, srv.URL)
	set := func(id, value string) {
		f.mu.Lock()
		defer f.mu.Unlock()
		f.keys[id]["value"] = value
		f.keys[id]["version"] = f.keys[id]["version"].(float64) + 1
	}
	resource.UnitTest(t, resource.TestCase{
		ProtoV6ProviderFactories: map[string]func() (tfprotov6.ProviderServer, error){"roll": providerserver.NewProtocol6WithError(New("test")())},
		Steps: []resource.TestStep{
			{
				Config: config,
				Check: resource.ComposeAggregateTestCheckFunc(
					resource.TestCheckResourceAttr("roll_secrets_environment.production", "name", "Production"),
					resource.TestCheckResourceAttr("roll_secrets_environment.production", "protected", "true"),
					resource.TestCheckResourceAttr("roll_secret.db", "version", "1"),
					resource.TestCheckResourceAttr("roll_secret.db", "path", "/"),
					resource.TestCheckResourceAttr("data.roll_secrets_values.production", "values.DB_URL", "postgres://db/shop"),
					resource.TestCheckResourceAttr("roll_secrets_identity.ci", "ttl_minutes", "15"),
					resource.TestCheckResourceAttr("roll_secrets_sync.deploys", "signing_secret", "whsec_once"),
					resource.TestCheckResourceAttr("roll_secrets_token.deploy", "token", "rlt_made_tok_1"),
					func(*terraform.State) error {
						f.mu.Lock()
						defer f.mu.Unlock()
						rule := f.tokens["tok_1"]["rules"].([]any)[0].(map[string]any)
						if fmt.Sprint(rule["resources"], rule["environments"], rule["paths"], rule["actions"]) != "[map[project:prj_a]] [production] [/api] map[read:allow reveal:allow]" || f.tokens["tok_1"]["expires_at"] != float64(1893456000000) {
							return fmt.Errorf("the token's rule: %v", f.tokens["tok_1"])
						}
						return nil
					},
				),
			},
			{
				// Changed outside Terraform: the next apply puts it back.
				PreConfig: func() { set("production:/DB_URL", "postgres://elsewhere") },
				Config:    config,
				Check: func(*terraform.State) error {
					f.mu.Lock()
					defer f.mu.Unlock()
					if got := f.keys["production:/DB_URL"]["value"]; got != "postgres://db/shop" {
						return fmt.Errorf("the value is %v", got)
					}
					return nil
				},
			},
		},
		CheckDestroy: func(*terraform.State) error {
			f.mu.Lock()
			defer f.mu.Unlock()
			if len(f.envs)+len(f.keys)+len(f.identities)+len(f.syncs) != 0 {
				return fmt.Errorf("left behind: %v %v %v %v", f.envs, f.keys, f.identities, f.syncs)
			}
			for id, t := range f.tokens {
				if t["revoked_at"] == nil {
					return fmt.Errorf("token %s wasn't revoked", id)
				}
			}
			return nil
		},
	})
}

// A value from an ephemeral read, written write-only: it reaches roll secrets and never Terraform's
// state, and the run's dynamic credential is revoked when it closes.
func TestValuesStayOutOfTheState(t *testing.T) {
	f := &fakeRolld{envs: map[string]*environment{"production": {Slug: "production", Name: "production"}, "staging": {Slug: "staging", Name: "staging"}}, keys: map[string]map[string]any{"production:/DB_URL": {"value": "postgres://db/shop", "version": float64(1), "comment": ""}}, identities: map[string]map[string]any{}, syncs: map[string]map[string]any{}, tokens: map[string]map[string]any{}}
	srv := httptest.NewServer(f)
	defer srv.Close()
	config := fmt.Sprintf(`
provider "roll" {
  api_url = %q
  token   = "rlt_terraform"
  project = "prj_a"
}
ephemeral "roll_secrets_values" "production" {
  environment = "production"
  dynamic     = true
}
resource "roll_secret" "copy" {
  environment      = "staging"
  key              = "DB_URL"
  value_wo         = ephemeral.roll_secrets_values.production.values["DB_URL"]
  value_wo_version = 1
}
`, srv.URL)
	resource.UnitTest(t, resource.TestCase{
		ProtoV6ProviderFactories: map[string]func() (tfprotov6.ProviderServer, error){"roll": providerserver.NewProtocol6WithError(New("test")())},
		TerraformVersionChecks:   []tfversion.TerraformVersionCheck{tfversion.SkipBelow(tfversion.Version1_11_0)},
		Steps: []resource.TestStep{{
			Config: config,
			Check: resource.ComposeAggregateTestCheckFunc(
				resource.TestCheckNoResourceAttr("roll_secret.copy", "value"),
				resource.TestCheckNoResourceAttr("roll_secret.copy", "value_wo"),
				func(s *terraform.State) error {
					f.mu.Lock()
					defer f.mu.Unlock()
					if got := f.keys["staging:/DB_URL"]["value"]; got != "postgres://db/shop" {
						return fmt.Errorf("roll secrets got %v", got)
					}
					if state := fmt.Sprintf("%#v", s.RootModule().Resources); strings.Contains(state, "postgres://") {
						return fmt.Errorf("the value is in the state")
					}
					if len(f.issued) == 0 || len(f.revoked) != len(f.issued) {
						return fmt.Errorf("leases issued %v, revoked %v", f.issued, f.revoked)
					}
					return nil
				},
			),
		}},
	})
}

// A machine client for CI: made trusting GitHub Actions' identity, its rules changed in place on
// its grant, its subject changed by a rotate (the client id stays), imported by its id, revoked.
func TestMachineClientAsCode(t *testing.T) {
	f := &fakeRolld{envs: map[string]*environment{}, keys: map[string]map[string]any{}, tokens: map[string]map[string]any{}, clients: map[string]map[string]any{}}
	srv := httptest.NewServer(f)
	defer srv.Close()
	// No subject: no credential at all, which the resource refuses (roll would make a secret).
	config := func(actions, subject string) string {
		oidc := ""
		if subject != "" {
			oidc = fmt.Sprintf(`
  oidc {
    issuer  = "https://token.actions.githubusercontent.com"
    subject = %q
    claims  = { repository_owner_id = "42" }
  }`, subject)
		}
		return fmt.Sprintf(`
provider "roll" {
  api_url = %q
  token   = "rlt_terraform"
  project = "prj_a"
}
resource "roll_machine_client" "ci" {
  name       = "web-ci"
  expires_at = "2030-01-01T00:00:00Z"
  rule {
    product   = "apps"
    resources = "*"
    actions   = { %s }
  }
  rule {
    product = "secrets"
    resource { project = "prj_a" }
    environments = ["production"]
    actions      = { read = "allow" }
  }%s
}
`, srv.URL, actions, oidc)
	}
	main, tag := "repo:acme/web:ref:refs/heads/main", "repo:acme/web:ref:refs/tags/*"
	// What the fake holds: the client's grant and its oidc, and how many times it rotated.
	held := func(check func(grant, oidc map[string]any, rotations int) error) resource.TestCheckFunc {
		return func(*terraform.State) error {
			f.mu.Lock()
			defer f.mu.Unlock()
			c := f.clients["mcl_1"]
			if len(f.clients) != 1 || c == nil {
				return fmt.Errorf("clients: %v", f.clients)
			}
			return check(f.tokens[c["grant"].(string)], c["oidc"].(map[string]any), len(f.rotations))
		}
	}
	resource.UnitTest(t, resource.TestCase{
		ProtoV6ProviderFactories: map[string]func() (tfprotov6.ProviderServer, error){"roll": providerserver.NewProtocol6WithError(New("test")())},
		Steps: []resource.TestStep{
			{Config: config(`read = "allow"`, ""), ExpectError: regexp.MustCompile(`(?s)oidc.*public_jwk`)},
			{
				Config: config(`read = "allow", deploy = "allow"`, main),
				Check: resource.ComposeAggregateTestCheckFunc(
					resource.TestCheckResourceAttr("roll_machine_client.ci", "client_id", "mcl_1"),
					resource.TestCheckResourceAttr("roll_machine_client.ci", "id", "mcl_1"),
					resource.TestCheckResourceAttr("roll_machine_client.ci", "grant_id", "tok_1"),
					resource.TestCheckResourceAttr("roll_machine_client.ci", "token_endpoint", srv.URL+"/v1/access/oauth/token"),
					resource.TestCheckResourceAttr("roll_machine_client.ci", "oidc.audience", srv.URL),
					held(func(grant, oidc map[string]any, _ int) error {
						rules, _ := json.Marshal(grant["rules"])
						want := `[{"actions":{"deploy":"allow","read":"allow"},"product":"apps","resources":"*"},{"actions":{"read":"allow"},"environments":["production"],"product":"secrets","resources":[{"project":"prj_a"}]}]`
						if string(rules) != want || grant["expires_at"] != float64(1893456000000) || oidc["subject"] != main || fmt.Sprint(oidc["claims"]) != "map[repository_owner_id:42]" {
							return fmt.Errorf("made %s, %v, %v", rules, grant, oidc)
						}
						return nil
					}),
				),
			},
			{
				// Rules change in place on the grant: no new client, no rotate.
				Config: config(`read = "allow", deploy = "allow", write = "ask"`, main),
				Check: resource.ComposeAggregateTestCheckFunc(
					resource.TestCheckResourceAttr("roll_machine_client.ci", "client_id", "mcl_1"),
					held(func(grant, _ map[string]any, rotations int) error {
						if rules := fmt.Sprint(grant["rules"]); !strings.Contains(rules, "write:ask") || rotations != 0 {
							return fmt.Errorf("rules %s after %d rotations", rules, rotations)
						}
						return nil
					}),
				),
			},
			{
				// A new subject is a new credential: rotated, the client id stays.
				Config: config(`read = "allow", deploy = "allow", write = "ask"`, tag),
				Check: resource.ComposeAggregateTestCheckFunc(
					resource.TestCheckResourceAttr("roll_machine_client.ci", "client_id", "mcl_1"),
					resource.TestCheckResourceAttr("roll_machine_client.ci", "oidc.subject", tag),
					held(func(_, oidc map[string]any, rotations int) error {
						if oidc["subject"] != tag || rotations != 1 {
							return fmt.Errorf("oidc %v after %d rotations", oidc, rotations)
						}
						return nil
					}),
				),
			},
			{ResourceName: "roll_machine_client.ci", ImportState: true, ImportStateVerify: true},
		},
		CheckDestroy: func(*terraform.State) error {
			f.mu.Lock()
			defer f.mu.Unlock()
			if f.tokens["tok_1"]["revoked_at"] == nil {
				return fmt.Errorf("the client wasn't revoked")
			}
			return nil
		},
	})
}

// What a project registers itself, as code: a CA, an SSH CA, an encryption key and a database
// outside roll, whose admin URL is write-only (sent once, never in the state).
func TestRegistrationsAsCode(t *testing.T) {
	f := &fakeRolld{envs: map[string]*environment{}, keys: map[string]map[string]any{}, identities: map[string]map[string]any{}, syncs: map[string]map[string]any{}, tokens: map[string]map[string]any{}}
	srv := httptest.NewServer(f)
	defer srv.Close()
	config := func(vault string) string {
		return fmt.Sprintf(`
provider "roll" {
  api_url = %q
  token   = "rlt_terraform"
  project = "prj_a"
}
resource "roll_secrets_ca" "internal" {
  name    = "internal"
  domains = ["Svc.Internal."]
}
resource "roll_secrets_ssh_ca" "fleet" {
  name         = "fleet"
  principals   = ["deploy"]
  host_domains = ["svc.internal"]
}
resource "roll_secrets_transit_key" "cards" {
  name                = "cards"
  deletion_protection = false
}
%s
resource "roll_secrets_outside_database" "rds" {
  name                 = "rds"
  admin_url_wo         = "postgres://admin:s3cret@db.example.com:5432/app"
  admin_url_wo_version = 1
  roles                = ["app_blue", "app_green"]
  ca                   = "-----BEGIN CERTIFICATE-----\n"
}
`, srv.URL, vault)
	}
	protected := `resource "roll_secrets_transit_key" "vault" {
  name = "vault"
}`
	unprotected := `resource "roll_secrets_transit_key" "vault" {
  name                = "vault"
  deletion_protection = false
}`
	resource.UnitTest(t, resource.TestCase{
		ProtoV6ProviderFactories: map[string]func() (tfprotov6.ProviderServer, error){"roll": providerserver.NewProtocol6WithError(New("test")())},
		TerraformVersionChecks:   []tfversion.TerraformVersionCheck{tfversion.SkipBelow(tfversion.Version1_11_0)},
		Steps: []resource.TestStep{{
			Config: config(protected),
			Check: resource.ComposeAggregateTestCheckFunc(
				resource.TestCheckResourceAttr("roll_secrets_ca.internal", "certificate", "-----BEGIN CERTIFICATE-----"),
				resource.TestCheckResourceAttr("roll_secrets_ca.internal", "max_days", "90"),
				resource.TestCheckResourceAttr("roll_secrets_ssh_ca.fleet", "public_key", "ssh-ed25519 AAAA roll-secrets:fleet"),
				resource.TestCheckResourceAttr("roll_secrets_transit_key.cards", "latest_version", "1"),
				resource.TestCheckResourceAttr("roll_secrets_outside_database.rds", "host", "db.example.com"),
				resource.TestCheckNoResourceAttr("roll_secrets_outside_database.rds", "admin_url_wo"),
				func(s *terraform.State) error {
					f.mu.Lock()
					defer f.mu.Unlock()
					if len(f.adminURLs) != 1 || f.adminURLs[0] != "postgres://admin:s3cret@db.example.com:5432/app" {
						return fmt.Errorf("registered with %v", f.adminURLs)
					}
					if strings.Contains(fmt.Sprintf("%#v", s.RootModule().Resources), "s3cret") {
						return fmt.Errorf("the admin URL is in the state")
					}
					return nil
				},
			),
		}, {
			// Values roll keeps normalised, as configured: nothing to change.
			Config:   config(protected),
			PlanOnly: true,
		}, {
			// A protected key isn't destroyed (a rename, or taking it out of the configuration)…
			Config:      config(""),
			ExpectError: regexp.MustCompile("deletion_protection"),
		}, {
			// …until protection is turned off, in place.
			Config: config(unprotected),
		}},
		CheckDestroy: func(*terraform.State) error {
			f.mu.Lock()
			defer f.mu.Unlock()
			for kind, items := range f.regs {
				if len(items) != 0 {
					return fmt.Errorf("%s left behind: %v", kind, items)
				}
			}
			return nil
		},
	})
}

// TestServeFakeRolld serves the fake on ROLL_FAKE_ROLLD_ADDR until the process is stopped, for
// sdk/pulumi's end-to-end run (its e2e.sh); GET /fake/state answers what it holds. Skipped otherwise.
func TestServeFakeRolld(t *testing.T) {
	addr := os.Getenv("ROLL_FAKE_ROLLD_ADDR")
	if addr == "" {
		t.Skip("ROLL_FAKE_ROLLD_ADDR isn't set")
	}
	f := &fakeRolld{envs: map[string]*environment{}, keys: map[string]map[string]any{}, identities: map[string]map[string]any{}, syncs: map[string]map[string]any{}, tokens: map[string]map[string]any{}}
	t.Fatal(http.ListenAndServe(addr, http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/fake/state" {
			f.ServeHTTP(w, r)
			return
		}
		f.mu.Lock()
		defer f.mu.Unlock()
		_ = json.NewEncoder(w).Encode(map[string]any{"environments": f.envs, "keys": f.keys, "tokens": f.tokens})
	})))
}
