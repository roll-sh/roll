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

// roll apps' API in memory (docs/apps/http-api.md): services, their production variables and
// custom domains, for one project, with the operator's token.
type fakeApps struct {
	mu       sync.Mutex
	next     int
	services map[string]map[string]any
	vars     map[string]map[string]any // svc/env/name → {value, sealed}
	domains  map[string]map[string]any // svc/env/host → Domain
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
			write(204, nil)
			return
		}
		write(200, s)
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

func TestAppsAsCode(t *testing.T) {
	f := &fakeApps{services: map[string]map[string]any{}, vars: map[string]map[string]any{}, domains: map[string]map[string]any{}}
	srv := httptest.NewServer(f)
	defer srv.Close()
	config := func(max int, level string) string {
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
`, srv.URL, max, level)
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
				Config: config(2, "info"),
				Check: resource.ComposeAggregateTestCheckFunc(
					resource.TestCheckResourceAttr("roll_app_service.web", "url", "https://web.up.roll.site"),
					resource.TestCheckResourceAttr("roll_app_service.web", "max_instances", "2"),
					resource.TestCheckResourceAttr("roll_app_service.web", "type", "web"),
					resource.TestCheckResourceAttr("roll_app_variable.key", "sealed", "true"),
					resource.TestCheckResourceAttr("roll_app_domain.www", "target", "edge.up.roll.site"),
					func(*terraform.State) error {
						if value("LOG_LEVEL") != "info" || value("API_KEY") != "sk_live_1" {
							return fmt.Errorf("the variables: %v", f.vars)
						}
						return nil
					},
				),
			},
			{
				// Changed: the service's size in place, the variable's value.
				Config: config(3, "debug"),
				Check: resource.ComposeAggregateTestCheckFunc(
					resource.TestCheckResourceAttr("roll_app_service.web", "max_instances", "3"),
					func(*terraform.State) error {
						if value("LOG_LEVEL") != "debug" {
							return fmt.Errorf("LOG_LEVEL is %v", value("LOG_LEVEL"))
						}
						return nil
					},
				),
			},
			{ResourceName: "roll_app_service.web", ImportState: true, ImportStateVerify: true},
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
			if len(f.services)+len(f.vars)+len(f.domains) != 0 {
				return fmt.Errorf("left behind: %v %v %v", f.services, f.vars, f.domains)
			}
			return nil
		},
	})
}
