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
	mu   sync.Mutex
	envs map[string]*environment
	keys map[string]map[string]any // env:path/key → {value|link, comment, version}
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
	f := &fakeRolld{envs: map[string]*environment{}, keys: map[string]map[string]any{}}
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
`, srv.URL)
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
			if len(f.envs)+len(f.keys) != 0 {
				return fmt.Errorf("left behind: %v %v", f.envs, f.keys)
			}
			return nil
		},
	})
}
