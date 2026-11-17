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

// roll apps' API in memory (docs/apps/http-api.md): services, their environments, variables,
// custom domains and links, and the project's notification targets, with the operator's token.
type fakeApps struct {
	mu       sync.Mutex
	next     int
	services map[string]map[string]any
	vars     map[string]map[string]any // svc/env/name → {value, sealed}
	domains  map[string]map[string]any // svc/env/host → Domain
	envs     map[string]map[string]any // svc/env → Environment (not production)
	links    map[string]map[string]any // svc/kind/ref → Link
	notify   map[string]map[string]any // id → NotifyTarget (+ url)
	logins   map[string]string         // svc → "user:token"
}

// project answers /v1/apps/projects/prj_a/…: its services and notification targets.
func (f *fakeApps) project(parts []string, method string, body map[string]any) (int, any) {
	if len(parts) < 2 || parts[0] != "prj_a" {
		return 404, map[string]string{"error": "no project"}
	}
	if parts[1] == "services" {
		list := []any{}
		for _, s := range f.services {
			list = append(list, s)
		}
		return 200, list
	}
	if len(parts) == 2 && method == http.MethodPost {
		f.next++
		id := fmt.Sprintf("ntf_%d", f.next)
		n := map[string]any{"id": id, "project": "prj_a", "kind": body["kind"], "host": "hooks.example.com", "events": []any{"deploy.failed", "crash_loop"}, "environments": []any{}, "services": []any{}, "previews": body["previews"], "url": body["url"]}
		for _, k := range []string{"events", "environments", "services"} {
			if body[k] != nil {
				n[k] = body[k]
			}
		}
		f.notify[id] = n
		out := map[string]any{"secret": "whsec_1"}
		for k, v := range n {
			if k != "url" {
				out[k] = v
			}
		}
		return 201, out
	}
	if len(parts) == 2 {
		list := []any{}
		for _, n := range f.notify {
			out := map[string]any{}
			for k, v := range n {
				if k != "url" {
					out[k] = v
				}
			}
			list = append(list, out)
		}
		return 200, list
	}
	n := f.notify[parts[2]]
	if n == nil {
		return 404, map[string]string{"error": "no target"}
	}
	switch method {
	case http.MethodDelete:
		delete(f.notify, parts[2])
		return 204, nil
	case http.MethodPatch:
		defaults := map[string]any{"events": []any{"deploy.failed", "crash_loop"}, "environments": []any{}, "services": []any{}}
		for k, v := range body {
			if v == nil {
				v = defaults[k]
			}
			n[k] = v
		}
	}
	return 200, n
}

func (f *fakeApps) ServeHTTP(w http.ResponseWriter, r *http.Request) {
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
	var body map[string]any
	_ = json.NewDecoder(r.Body).Decode(&body)
	if p, ok := strings.CutPrefix(r.URL.Path, "/v1/apps/projects/"); ok {
		code, v := f.project(strings.Split(p, "/"), r.Method, body)
		write(code, v)
		return
	}
	rest := strings.TrimPrefix(r.URL.Path, "/v1/apps/services")
	parts := strings.Split(strings.Trim(rest, "/"), "/")
	if rest == "" && r.Method == http.MethodPost {
		f.next++
		id := fmt.Sprintf("svc_%012d", f.next)
		run := map[string]any{"type": "web", "min_instances": 0, "max_instances": 3, "memory_mb": 512}
		for k, v := range body["run"].(map[string]any) {
			run[k] = v
		}
		s := map[string]any{"id": id, "project": body["project"], "name": body["name"], "source": body["source"], "run": run, "environments": []any{map[string]any{"name": "production", "url": fmt.Sprintf("https://%s.up.roll.site", body["name"])}}}
		f.services[id] = s
		if l, ok := body["git_login"].(map[string]any); ok {
			f.logins[id] = fmt.Sprintf("%s:%s", l["username"], l["password"])
			s["git_login"] = map[string]any{"host": "gitlab.example.com", "username": l["username"]}
		}
		write(201, s)
		return
	}
	s := f.services[parts[0]]
	if s == nil {
		write(404, map[string]string{"error": "no service " + parts[0]})
		return
	}
	if len(parts) == 1 {
		switch r.Method {
		case http.MethodPatch:
			s["name"], s["source"] = body["name"], body["source"]
			for k, v := range body["run"].(map[string]any) {
				s["run"].(map[string]any)[k] = v
			}
		case http.MethodDelete:
			delete(f.services, parts[0])
			delete(f.logins, parts[0])
			write(204, nil)
			return
		}
		write(200, s)
		return
	}
	if parts[1] == "git-login" {
		if r.Method == http.MethodDelete {
			delete(f.logins, parts[0])
			s["git_login"] = nil
		} else {
			f.logins[parts[0]] = fmt.Sprintf("%s:%s", body["username"], body["password"])
			s["git_login"] = map[string]any{"host": "gitlab.example.com", "username": body["username"]}
		}
		write(200, s)
		return
	}
	if parts[1] == "links" {
		if len(parts) == 2 {
			list := []any{}
			for id, l := range f.links {
				if strings.HasPrefix(id, parts[0]+"/") {
					list = append(list, l)
				}
			}
			write(200, list)
			return
		}
		if len(parts) != 4 {
			write(404, map[string]string{"error": "no route"})
			return
		}
		id := parts[0] + "/" + parts[2] + "/" + parts[3]
		if r.Method == http.MethodDelete {
			delete(f.links, id)
			write(204, nil)
			return
		}
		prefix, _ := body["prefix"].(string)
		if prefix == "" {
			prefix = "ORDERS"
		}
		l := map[string]any{"kind": parts[2], "ref": parts[3], "prefix": prefix, "vars": []any{prefix + "_DATABASE_URL"}}
		f.links[id] = l
		write(200, l)
		return
	}
	// …/environments/{env}: another environment.
	if len(parts) == 3 && parts[1] == "environments" {
		id := parts[0] + "/" + parts[2]
		switch r.Method {
		case http.MethodPut:
			e := f.envs[id]
			if e == nil {
				e = map[string]any{"name": parts[2], "url": fmt.Sprintf("https://%s-%s.up.roll.site", s["name"], parts[2]), "wait_for_ci": false, "branch": nil}
				f.envs[id] = e
			}
			for k, v := range body {
				e[k] = v
			}
		case http.MethodDelete:
			delete(f.envs, id)
			write(204, nil)
			return
		}
		if f.envs[id] == nil {
			write(404, map[string]string{"error": "no environment"})
			return
		}
		write(200, f.envs[id])
		return
	}
	// …/environments/{env}/variables | /domains[/host]
	if len(parts) < 4 || parts[1] != "environments" {
		write(404, map[string]string{"error": "no route"})
		return
	}
	base := parts[0] + "/" + parts[2] + "/"
	switch parts[3] {
	case "variables":
		if r.Method == http.MethodPatch {
			set, _ := body["set"].(map[string]any)
			for k, v := range set {
				sealed := f.vars[base+k] != nil && f.vars[base+k]["sealed"] == true
				f.vars[base+k] = map[string]any{"value": v, "sealed": sealed}
			}
			for _, k := range asList(body["seal"]) {
				f.vars[base+k]["sealed"] = true
			}
			for _, k := range asList(body["unset"]) {
				delete(f.vars, base+k)
			}
		}
		list := []any{}
		for id, v := range f.vars {
			if name, ok := strings.CutPrefix(id, base); ok {
				list = append(list, map[string]any{"name": name, "masked": "••••", "source": "service", "sealed": v["sealed"]})
			}
		}
		write(200, map[string]any{"vars": list})
	case "domains":
		if len(parts) == 5 && r.Method == http.MethodDelete {
			delete(f.domains, base+parts[4])
			write(204, nil)
			return
		}
		if r.Method == http.MethodPost {
			d := map[string]any{"hostname": body["hostname"], "status": "pending", "target": "edge.up.roll.site"}
			f.domains[base+body["hostname"].(string)] = d
			write(201, d)
			return
		}
		list := []any{}
		for id, d := range f.domains {
			if strings.HasPrefix(id, base) {
				list = append(list, d)
			}
		}
		write(200, list)
	}
}

func asList(v any) []string {
	out := []string{}
	for _, x := range func() []any { l, _ := v.([]any); return l }() {
		out = append(out, x.(string))
	}
	return out
}

// The private Git service, environment, link, notification target and data source each step
// adds: the Git token, the branch, the link's prefix line, the target's filter line, and how the
// data source names the service.
const more = `
resource "roll_app_service" "site" {
  name         = "site"
  git_url      = "https://gitlab.example.com/acme/site"
  git_username = "deploy"
  git_token    = %q
}
resource "roll_app_environment" "staging" {
  service = roll_app_service.web.id
  name    = "staging"
  branch  = %q
}
resource "roll_app_link" "orders" {
  service = roll_app_service.web.id
  kind    = "postgres"
  ref     = "orders"
  %s
}
resource "roll_app_notification" "hook" {
  url = "https://hooks.example.com/roll"
  %s
}
data "roll_app_service" "web" {
  %s
  depends_on = [roll_app_service.web]
}
`

// loginIs checks the Git login the fake keeps for the service named `name`.
func loginIs(f *fakeApps, name, want string) error {
	f.mu.Lock()
	defer f.mu.Unlock()
	for id, s := range f.services {
		if s["name"] == name && f.logins[id] == want {
			return nil
		}
	}
	return fmt.Errorf("%s's login isn't %s: %v", name, want, f.logins)
}

func idOf(name string) resource.ImportStateIdFunc {
	return func(s *terraform.State) (string, error) {
		return s.RootModule().Resources[name].Primary.Attributes["id"], nil
	}
}

func TestAppsAsCode(t *testing.T) {
	f := &fakeApps{services: map[string]map[string]any{}, vars: map[string]map[string]any{}, domains: map[string]map[string]any{}, envs: map[string]map[string]any{}, links: map[string]map[string]any{}, notify: map[string]map[string]any{}, logins: map[string]string{}}
	srv := httptest.NewServer(f)
	defer srv.Close()
	config := func(max int, level string, more string) string {
		return fmt.Sprintf(`
provider "roll" {
  api_url = %q
  token   = "rlt_terraform"
  project = "prj_a"
}
resource "roll_app_service" "web" {
  name          = "web"
  image         = "nginx:1.27"
  port          = 80
  max_instances = %d
}
resource "roll_app_variable" "level" {
  service = roll_app_service.web.id
  name    = "LOG_LEVEL"
  value   = %q
}
resource "roll_app_variable" "key" {
  service = roll_app_service.web.id
  name    = "API_KEY"
  value   = "sk_live_1"
  sealed  = true
}
resource "roll_app_domain" "www" {
  service  = roll_app_service.web.id
  hostname = "www.example.com"
}
%s
`, srv.URL, max, level, more)
	}
	value := func(k string) any {
		f.mu.Lock()
		defer f.mu.Unlock()
		for id, v := range f.vars {
			if strings.HasSuffix(id, "/production/"+k) {
				return v["value"]
			}
		}
		return nil
	}
	resource.UnitTest(t, resource.TestCase{
		ProtoV6ProviderFactories: map[string]func() (tfprotov6.ProviderServer, error){"roll": providerserver.NewProtocol6WithError(New("test")())},
		Steps: []resource.TestStep{
			{
				Config: config(2, "info", fmt.Sprintf(more, "glpat-1", "main", `prefix = "ORDERS"`, `events = ["deploy.failed"]`, "id = roll_app_service.web.id")),
				Check: resource.ComposeAggregateTestCheckFunc(
					resource.TestCheckResourceAttr("roll_app_service.web", "url", "https://web.up.roll.site"),
					resource.TestCheckResourceAttr("roll_app_service.web", "max_instances", "2"),
					resource.TestCheckResourceAttr("roll_app_service.web", "type", "web"),
					resource.TestCheckResourceAttr("roll_app_variable.key", "sealed", "true"),
					resource.TestCheckResourceAttr("roll_app_domain.www", "target", "edge.up.roll.site"),
					resource.TestCheckResourceAttr("roll_app_environment.staging", "url", "https://web-staging.up.roll.site"),
					resource.TestCheckResourceAttr("roll_app_environment.staging", "branch", "main"),
					func(*terraform.State) error { return loginIs(f, "site", "deploy:glpat-1") },
					resource.TestCheckResourceAttr("roll_app_link.orders", "vars.0", "ORDERS_DATABASE_URL"),
					resource.TestCheckResourceAttr("roll_app_notification.hook", "secret", "whsec_1"),
					resource.TestCheckResourceAttr("roll_app_notification.hook", "events.#", "1"),
					resource.TestCheckResourceAttr("data.roll_app_service.web", "environments.production", "https://web.up.roll.site"),
					func(*terraform.State) error {
						if value("LOG_LEVEL") != "info" || value("API_KEY") != "sk_live_1" {
							return fmt.Errorf("the variables: %v", f.vars)
						}
						return nil
					},
				),
			},
			{
				// Changed in place: the service's size, the variable's value, the environment's branch, the link's prefix and the target's filters (its secret stays).
				Config: config(3, "debug", fmt.Sprintf(more, "glpat-2", "develop", `prefix = "SHOP"`, `environments = ["production"]`, `name = "web"`)),
				Check: resource.ComposeAggregateTestCheckFunc(
					resource.TestCheckResourceAttr("roll_app_service.web", "max_instances", "3"),
					resource.TestCheckResourceAttr("roll_app_environment.staging", "branch", "develop"),
					func(*terraform.State) error { return loginIs(f, "site", "deploy:glpat-2") },
					resource.TestCheckResourceAttr("roll_app_link.orders", "vars.0", "SHOP_DATABASE_URL"),
					resource.TestCheckResourceAttr("roll_app_notification.hook", "environments.0", "production"),
					resource.TestCheckResourceAttr("roll_app_notification.hook", "events.#", "2"),
					resource.TestCheckResourceAttr("roll_app_notification.hook", "secret", "whsec_1"),
					resource.TestCheckResourceAttrPair("data.roll_app_service.web", "id", "roll_app_service.web", "id"),
					func(*terraform.State) error {
						if value("LOG_LEVEL") != "debug" {
							return fmt.Errorf("LOG_LEVEL is %v", value("LOG_LEVEL"))
						}
						return nil
					},
				),
			},
			{ResourceName: "roll_app_service.web", ImportState: true, ImportStateVerify: true},
			{ResourceName: "roll_app_environment.staging", ImportState: true, ImportStateVerify: true, ImportStateVerifyIgnore: []string{"variables_from"}, ImportStateIdFunc: idOf("roll_app_environment.staging")},
			{ResourceName: "roll_app_link.orders", ImportState: true, ImportStateVerify: true, ImportStateIdFunc: idOf("roll_app_link.orders")},
			{ResourceName: "roll_app_notification.hook", ImportState: true, ImportStateVerify: true, ImportStateVerifyIgnore: []string{"url", "secret"}, ImportStateIdFunc: func(s *terraform.State) (string, error) {
				a := s.RootModule().Resources["roll_app_notification.hook"].Primary.Attributes
				return a["project"] + "/" + a["id"], nil
			}},
			{
				ResourceName:      "roll_app_domain.www",
				ImportState:       true,
				ImportStateVerify: true,
				ImportStateIdFunc: func(s *terraform.State) (string, error) {
					return s.RootModule().Resources["roll_app_domain.www"].Primary.Attributes["id"], nil
				},
			},
		},
		CheckDestroy: func(*terraform.State) error {
			f.mu.Lock()
			defer f.mu.Unlock()
			if len(f.services)+len(f.vars)+len(f.domains)+len(f.envs)+len(f.links)+len(f.notify)+len(f.logins) != 0 {
				return fmt.Errorf("left behind: %v %v %v %v %v %v", f.services, f.vars, f.domains, f.envs, f.links, f.notify)
			}
			return nil
		},
	})
}
