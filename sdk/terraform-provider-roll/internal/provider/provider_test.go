package provider

import (
	"encoding/json"
	"fmt"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync"
	"testing"

	"github.com/hashicorp/terraform-plugin-framework/providerserver"
	"github.com/hashicorp/terraform-plugin-go/tfprotov6"
	"github.com/hashicorp/terraform-plugin-testing/helper/resource"
	"github.com/hashicorp/terraform-plugin-testing/terraform"
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
	default:
		write(200, f.tokens[id])
	}
}

// rest serves identities and syncs (create, list, delete), as roll keeps them.
func (f *fakeRolld) rest(w http.ResponseWriter, r *http.Request, kind string, items map[string]map[string]any, write func(int, any)) {
	id := strings.TrimPrefix(strings.TrimPrefix(r.URL.Path, "/v1/secrets/projects/prj_a/"+kind), "/")
	switch {
	case r.Method == http.MethodPost && id == "":
		var b map[string]any
		_ = json.NewDecoder(r.Body).Decode(&b)
		b["id"] = fmt.Sprintf("%s_%d", kind[:3], len(items)+1)
		if kind == "identities" && b["ttl_minutes"] == nil {
			b["ttl_minutes"] = 15
		}
		items[b["id"].(string)] = b
		answer := map[string]any{}
		for k, v := range b {
			answer[k] = v
		}
		if kind == "syncs" && b["kind"] == "webhook" {
			answer["signing_secret"] = "whsec_once"
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
	for _, kind := range []string{"identities", "syncs"} {
		if strings.HasPrefix(r.URL.Path, "/v1/secrets/projects/prj_a/"+kind) {
			items := map[string]map[string]map[string]any{"identities": f.identities, "syncs": f.syncs}[kind]
			f.rest(w, r, kind, items, write)
			return
		}
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
		write(200, map[string]any{"values": values, "problems": []any{}})
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
