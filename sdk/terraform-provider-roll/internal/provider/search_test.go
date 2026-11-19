package provider

import (
	"encoding/base64"
	"encoding/json"
	"fmt"
	"net/http"
	"net/http/httptest"
	"net/netip"
	"os"
	"path/filepath"
	"regexp"
	"strings"
	"sync"
	"testing"

	"github.com/hashicorp/terraform-plugin-framework/providerserver"
	"github.com/hashicorp/terraform-plugin-go/tfprotov6"
	"github.com/hashicorp/terraform-plugin-testing/helper/resource"
	"github.com/hashicorp/terraform-plugin-testing/plancheck"
	"github.com/hashicorp/terraform-plugin-testing/terraform"
	"github.com/hashicorp/terraform-plugin-testing/tfjsonpath"
	"github.com/hashicorp/terraform-plugin-testing/tfversion"
)

// rolld's /v1/search in memory, answering as crates/search-control/src/api.rs does: clusters
// (held to a person's caps), keys, and the console's `_security/api_key` and `_snapshot` calls.
type fakeSearch struct {
	mu       sync.Mutex
	clusters map[string]*fakeCluster // by id
	deleted  []string
	// A person's caps (0: none), as --search-max-clusters-per-person and …-always-on-….
	capClusters, capAlwaysOn int
	// Keys revoked through the keys route, and repositories' credentials as roll sealed them.
	revoked []string
	sealed  map[string]map[string]any
}

type fakeCluster struct {
	id, name, flavor, password string
	idle, maxInFlight, maxBody int64
	drn                        bool
	allow                      []map[string]any
	loaded                     bool
	keys                       map[string]bool // id → invalidated
	repos                      map[string]map[string]any
}

func newFakeSearch() *fakeSearch {
	return &fakeSearch{clusters: map[string]*fakeCluster{}, sealed: map[string]map[string]any{}}
}

func (c *fakeCluster) view(withPassword bool) map[string]any {
	state := map[string]any{"state": "asleep"}
	if c.loaded {
		state = map[string]any{"state": "loaded", "in_flight": 0}
	}
	v := map[string]any{
		"id": c.id, "name": c.name, "region": "local", "flavor": c.flavor, "idle_timeout_s": c.idle, "max_in_flight": c.maxInFlight,
		"max_body_mb": c.maxBody, "destructive_requires_name": c.drn, "ip_allowlist": c.allow, "owner": "usr_1", "parent_id": nil,
		"url": "https://" + c.name + ".search.roll.example", "host": c.name + ".search.roll.example", "port": 443, "username": c.name,
		"status": state, "parent_name": nil, "branches": []any{},
	}
	if withPassword {
		v["password"] = c.password
	}
	return v
}

func (f *fakeSearch) byIDOrName(key string) *fakeCluster {
	if c := f.clusters[key]; c != nil {
		return c
	}
	for _, c := range f.clusters {
		if c.name == key {
			return c
		}
	}
	return nil
}

// apply is Control::apply and check: the options given, validated as roll validates them.
func (f *fakeSearch) apply(c *fakeCluster, b map[string]any) string {
	if v, ok := b["flavor"].(string); ok {
		c.flavor = strings.ToLower(v)
	}
	if v, ok := b["idle_timeout_s"].(float64); ok {
		c.idle = int64(v)
	}
	if v, ok := b["max_in_flight"].(float64); ok {
		c.maxInFlight = int64(v)
	}
	if v, ok := b["max_body_mb"].(float64); ok {
		c.maxBody = int64(v)
	}
	if v, ok := b["destructive_requires_name"].(bool); ok {
		c.drn = v
	}
	if list, ok := b["ip_allowlist"].([]any); ok {
		c.allow = []map[string]any{}
		for _, x := range list {
			r := map[string]any{}
			switch x := x.(type) {
			case string:
				r["cidr"] = x
			case map[string]any:
				r["cidr"] = x["cidr"]
				if d, _ := x["description"].(string); strings.TrimSpace(d) != "" {
					r["description"] = strings.TrimSpace(d)
				}
			}
			s, _ := r["cidr"].(string)
			p, err := netip.ParsePrefix(s)
			if err != nil {
				a, err := netip.ParseAddr(s)
				if err != nil {
					return "ip_allowlist: invalid CIDR " + s
				}
				p = netip.PrefixFrom(a, a.BitLen())
			}
			r["cidr"] = p.Masked().String()
			c.allow = append(c.allow, r)
		}
	}
	if c.flavor != "elasticsearch" && c.flavor != "opensearch" {
		return "unknown flavor " + c.flavor + ": use elasticsearch or opensearch"
	}
	if c.maxInFlight < 1 || c.maxInFlight > 4096 {
		return "max_in_flight must be between 1 and 4096"
	}
	return ""
}

// over is the 409 a person's create or always-on switch gets past a cap (control.rs `over`).
func (f *fakeSearch) over(c *fakeCluster, adding bool) string {
	n, on := 0, 0
	for _, x := range f.clusters {
		if x.id == c.id {
			continue
		}
		n++
		if x.idle == 0 {
			on++
		}
	}
	if adding && f.capClusters > 0 && n >= f.capClusters {
		return fmt.Sprintf("you have %d search clusters (branches included) and a person may have %d: delete one, or the operator can raise the cap with --search-max-clusters-per-person", n, f.capClusters)
	}
	if c.idle == 0 && f.capAlwaysOn > 0 && on >= f.capAlwaysOn {
		return fmt.Sprintf("you have %d always-on search clusters (idle_timeout_s 0) and a person may have %d: a cluster that sleeps when idle (idle_timeout_s above 0) doesn't count, or the operator can raise the cap with --search-max-always-on-per-person", on, f.capAlwaysOn)
	}
	return ""
}

func (f *fakeSearch) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	f.mu.Lock()
	defer f.mu.Unlock()
	write := func(code int, v any) {
		w.Header().Set("Content-Type", "application/json")
		w.WriteHeader(code)
		if v != nil {
			_ = json.NewEncoder(w).Encode(v)
		}
	}
	fail := func(code int, msg string) { write(code, map[string]string{"error": msg}) }
	if r.Header.Get("Authorization") != "Bearer rlt_terraform" {
		fail(401, "no token")
		return
	}
	var b map[string]any
	_ = json.NewDecoder(r.Body).Decode(&b)
	if r.URL.Path == "/v1/search" {
		write(200, map[string]any{"version": "9.5.5", "opensearch_version": "3.9.0", "deleted_retention_s": 604800,
			"defaults": map[string]any{"flavor": "elasticsearch", "idle_timeout_s": 900, "max_in_flight": 64, "max_body_mb": 100, "destructive_requires_name": true}})
		return
	}
	if r.URL.Path == "/v1/search/clusters" && r.Method == http.MethodPost {
		name, _ := b["name"].(string)
		c := &fakeCluster{id: fmt.Sprintf("srch_%d", len(f.clusters)+len(f.deleted)+1), name: name, flavor: "elasticsearch", password: "pw-" + name,
			idle: 900, maxInFlight: 64, maxBody: 100, drn: true, allow: []map[string]any{}, keys: map[string]bool{}, repos: map[string]map[string]any{}}
		if msg := f.apply(c, b); msg != "" {
			fail(400, msg)
			return
		}
		if f.byIDOrName(name) != nil {
			fail(409, "a search cluster named "+name+" already exists")
			return
		}
		if msg := f.over(c, true); msg != "" {
			fail(409, msg)
			return
		}
		// An always-on cluster loads as it's made.
		c.loaded = c.idle == 0
		f.clusters[c.id] = c
		write(200, c.view(true))
		return
	}
	rest, ok := strings.CutPrefix(r.URL.Path, "/v1/search/clusters/")
	if !ok {
		fail(404, "no route")
		return
	}
	key, sub, _ := strings.Cut(rest, "/")
	c := f.byIDOrName(key)
	if c == nil {
		fail(404, "no search cluster "+key)
		return
	}
	switch {
	case sub == "" && r.Method == http.MethodGet:
		write(200, c.view(false))
	case sub == "" && r.Method == http.MethodPatch:
		next := *c
		if msg := f.apply(&next, b); msg != "" {
			fail(400, msg)
			return
		}
		if msg := f.over(&next, false); msg != "" {
			fail(409, msg)
			return
		}
		// A new flavor keeps the cluster and its data; keys don't carry over (an API key isn't an
		// OpenSearch user).
		if next.flavor != c.flavor {
			next.keys = map[string]bool{}
		}
		*c = next
		c.loaded = c.loaded || c.idle == 0
		write(200, c.view(false))
	case sub == "" && r.Method == http.MethodDelete:
		delete(f.clusters, c.id)
		f.deleted = append(f.deleted, c.id)
		write(200, map[string]any{"deleted": c.id, "name": c.name})
	case sub == "keys" && r.Method == http.MethodPost:
		id := fmt.Sprintf("key%d", len(c.keys)+1)
		v := map[string]any{"name": b["name"], "access": b["access"], "url": "https://" + c.name + ".search.roll.example"}
		if c.flavor == "opensearch" {
			id = "roll-key-" + id
			v["username"], v["password"] = id+"@"+c.name, "secret-"+id
		} else {
			v["api_key"] = base64.StdEncoding.EncodeToString([]byte(id + "@" + c.name + ":secret-" + id))
		}
		c.keys[id] = false
		v["id"] = id
		if s, ok := b["expires_in_s"].(float64); ok {
			v["expires_at"] = 1893456000 + int64(s)
		}
		write(200, v)
	case strings.HasPrefix(sub, "keys/") && r.Method == http.MethodDelete:
		id := strings.TrimPrefix(sub, "keys/")
		if invalidated, ok := c.keys[id]; !ok || invalidated {
			fail(404, "no API key "+id+" on cluster "+c.name)
			return
		}
		c.keys[id] = true
		f.revoked = append(f.revoked, id)
		write(204, nil)
	case sub == "request" && r.Method == http.MethodPost:
		status, body := f.console(c, b)
		out, _ := json.Marshal(body)
		c.loaded = true
		write(200, map[string]any{"status": status, "headers": []any{}, "body": string(out), "json": true, "took_ms": 1, "truncated": false})
	default:
		fail(404, "no route")
	}
}

// console answers one Elasticsearch request as the cluster would.
func (f *fakeSearch) console(c *fakeCluster, req map[string]any) (int, any) {
	method, _ := req["method"].(string)
	p, _ := req["path"].(string)
	body, _ := req["body"].(map[string]any)
	esErr := func(status int, typ, reason string) (int, any) {
		return status, map[string]any{"error": map[string]any{"type": typ, "reason": reason}, "status": status}
	}
	if c.flavor == "opensearch" && strings.HasPrefix(p, "/_security") {
		return esErr(400, "illegal_argument_exception", "no handler found for uri ["+p+"]")
	}
	switch {
	case method == "POST" && p == "/_security/api_key":
		if body["role_descriptors"] == nil {
			return esErr(400, "action_request_validation_exception", "role_descriptors expected")
		}
		id := fmt.Sprintf("key%d", len(c.keys)+1)
		c.keys[id] = false
		out := map[string]any{"id": id, "name": body["name"], "api_key": "secret-" + id, "encoded": base64.StdEncoding.EncodeToString([]byte(id + ":secret-" + id))}
		if body["expiration"] == "2592000s" {
			out["expiration"] = int64(1893456000000 + 2592000000)
		}
		return 200, out
	case method == "GET" && strings.HasPrefix(p, "/_security/api_key?id="):
		id := strings.TrimPrefix(p, "/_security/api_key?id=")
		invalidated, ok := c.keys[id]
		if !ok {
			return esErr(404, "resource_not_found_exception", "api key ["+id+"] not found")
		}
		return 200, map[string]any{"api_keys": []any{map[string]any{"id": id, "invalidated": invalidated}}}
	case strings.HasPrefix(p, "/_snapshot/"):
		name := strings.TrimPrefix(p, "/_snapshot/")
		repo := c.repos[name]
		switch {
		case method == "PUT":
			settings, _ := body["settings"].(map[string]any)
			sealed := map[string]any{}
			for _, k := range []string{"access_key", "secret_key", "session_token", "credentials_file", "account", "key", "sas_token"} {
				if v, ok := settings[k]; ok {
					sealed[k] = v
					delete(settings, k)
				}
			}
			f.sealed[c.id+"/"+name] = sealed
			c.repos[name] = map[string]any{"type": body["type"], "settings": settings}
			return 200, map[string]any{"acknowledged": true}
		case repo == nil:
			return esErr(404, "repository_missing_exception", "["+name+"] missing")
		case method == "GET":
			return 200, map[string]any{name: repo}
		case method == "DELETE":
			delete(c.repos, name)
			delete(f.sealed, c.id+"/"+name)
			return 200, map[string]any{"acknowledged": true}
		}
	}
	return esErr(400, "illegal_argument_exception", "no handler found for uri ["+p+"] and method ["+method+"]")
}

func searchFactories() map[string]func() (tfprotov6.ProviderServer, error) {
	return map[string]func() (tfprotov6.ProviderServer, error){"roll": providerserver.NewProtocol6WithError(New("test")())}
}

// held runs check on the fake's cluster named name.
func held(f *fakeSearch, name string, check func(*fakeCluster) error) resource.TestCheckFunc {
	return func(*terraform.State) error {
		f.mu.Lock()
		defer f.mu.Unlock()
		c := f.byIDOrName(name)
		if c == nil {
			return fmt.Errorf("no cluster %s: %v", name, f.clusters)
		}
		return check(c)
	}
}

// A search cluster as code: made with a key and a snapshot repository, changed in place,
// switched to another flavor in place (its key made again), imported, destroyed. The keys'
// secrets stay out of the plan.
func TestSearchAsCode(t *testing.T) {
	f := newFakeSearch()
	srv := httptest.NewServer(f)
	defer srv.Close()
	config := func(shop, logsFlavor string) string {
		return fmt.Sprintf(`
provider "roll" {
  api_url = %q
  token   = "rlt_terraform"
}
resource "roll_search_cluster" "shop" {
  name = "shop"
  %s
}
resource "roll_search_api_key" "reader" {
  cluster          = roll_search_cluster.shop.id
  name             = "reader"
  role_descriptors = jsonencode({ reader = { indices = [{ names = ["products"], privileges = ["read"] }] } })
  expiration       = "30d"
}
resource "roll_search_snapshot_repository" "backups" {
  cluster     = roll_search_cluster.shop.id
  name        = "backups"
  type        = "s3"
  settings    = { bucket = "acme-backups", base_path = "shop" }
  credentials = { access_key = "AKIAEXAMPLE", secret_key = "s3cret-key" }
}
resource "roll_search_cluster" "logs" {
  name   = "logs"
  flavor = %q
}
resource "roll_search_api_key" "shipper" {
  cluster = roll_search_cluster.logs.id
  name    = "shipper"
  lifecycle { replace_triggered_by = [roll_search_cluster.logs.flavor] }
}
data "roll_search_cluster" "shop" {
  name       = "shop"
  depends_on = [roll_search_cluster.shop]
}
`, srv.URL, shop, logsFlavor)
	}
	first := `ip_allowlist = [{ cidr = "10.1.2.3/8", description = "office" }]`
	changed := `always_on     = true
  max_in_flight = 32
  ip_allowlist  = [{ cidr = "203.0.113.7" }, { cidr = "2001:db8::1/32", description = "v6" }]`
	var shopID, logsID string
	resource.UnitTest(t, resource.TestCase{
		ProtoV6ProviderFactories: searchFactories(),
		Steps: []resource.TestStep{
			{
				Config: config(first, "elasticsearch"),
				ConfigPlanChecks: resource.ConfigPlanChecks{PreApply: []plancheck.PlanCheck{
					plancheck.ExpectSensitiveValue("roll_search_api_key.reader", tfjsonpath.New("encoded")),
					plancheck.ExpectSensitiveValue("roll_search_api_key.shipper", tfjsonpath.New("encoded")),
					plancheck.ExpectSensitiveValue("roll_search_snapshot_repository.backups", tfjsonpath.New("credentials")),
				}},
				Check: resource.ComposeAggregateTestCheckFunc(
					resource.TestCheckResourceAttr("roll_search_cluster.shop", "flavor", "elasticsearch"),
					resource.TestCheckResourceAttr("roll_search_cluster.shop", "endpoint", "https://shop.search.roll.example"),
					resource.TestCheckResourceAttr("roll_search_cluster.shop", "idle_timeout_s", "900"),
					resource.TestCheckResourceAttr("roll_search_cluster.shop", "always_on", "false"),
					resource.TestCheckResourceAttr("roll_search_cluster.shop", "version", "9.5.5"),
					resource.TestCheckResourceAttr("roll_search_cluster.shop", "password", "pw-shop"),
					// As written, while roll keeps it canonical.
					resource.TestCheckResourceAttr("roll_search_cluster.shop", "ip_allowlist.0.cidr", "10.1.2.3/8"),
					held(f, "shop", func(c *fakeCluster) error {
						if fmt.Sprint(c.allow) != "[map[cidr:10.0.0.0/8 description:office]]" {
							return fmt.Errorf("the allowlist: %v", c.allow)
						}
						return nil
					}),
					resource.TestCheckResourceAttr("roll_search_api_key.reader", "encoded", base64.StdEncoding.EncodeToString([]byte("key1@shop:secret-key1"))),
					resource.TestCheckResourceAttr("roll_search_api_key.reader", "expires_at", "2030-01-31T00:00:00Z"),
					resource.TestCheckNoResourceAttr("roll_search_api_key.shipper", "username"),
					resource.TestCheckResourceAttrSet("roll_search_api_key.shipper", "encoded"),
					resource.TestCheckResourceAttr("roll_search_snapshot_repository.backups", "settings.bucket", "acme-backups"),
					held(f, "shop", func(c *fakeCluster) error {
						got := f.sealed[c.id+"/backups"]
						if got["secret_key"] != "s3cret-key" || c.repos["backups"]["settings"].(map[string]any)["secret_key"] != nil {
							return fmt.Errorf("the credentials weren't sealed apart: %v %v", got, c.repos)
						}
						return nil
					}),
					resource.TestCheckResourceAttr("data.roll_search_cluster.shop", "ip_allowlist.0.cidr", "10.0.0.0/8"),
					resource.TestCheckResourceAttrPair("data.roll_search_cluster.shop", "id", "roll_search_cluster.shop", "id"),
					func(s *terraform.State) error {
						shopID = s.RootModule().Resources["roll_search_cluster.shop"].Primary.ID
						logsID = s.RootModule().Resources["roll_search_cluster.logs"].Primary.ID
						return nil
					},
				),
			},
			{
				// The same configuration, refreshed with every cluster loaded: nothing to change.
				PreConfig: func() {
					f.mu.Lock()
					defer f.mu.Unlock()
					for _, c := range f.clusters {
						c.loaded = true
					}
				},
				Config:   config(first, "elasticsearch"),
				PlanOnly: true,
			},
			{
				// In place: always on, fewer requests at once, another allowlist.
				Config: config(changed, "elasticsearch"),
				ConfigPlanChecks: resource.ConfigPlanChecks{PreApply: []plancheck.PlanCheck{
					plancheck.ExpectResourceAction("roll_search_cluster.shop", plancheck.ResourceActionUpdate),
					plancheck.ExpectResourceAction("roll_search_api_key.reader", plancheck.ResourceActionNoop),
				}},
				Check: resource.ComposeAggregateTestCheckFunc(
					resource.TestCheckResourceAttr("roll_search_cluster.shop", "idle_timeout_s", "0"),
					resource.TestCheckResourceAttr("roll_search_cluster.shop", "ip_allowlist.0.cidr", "203.0.113.7"),
					resource.TestCheckResourceAttr("roll_search_cluster.shop", "password", "pw-shop"),
					held(f, "shop", func(c *fakeCluster) error {
						if c.id != shopID || c.idle != 0 || c.maxInFlight != 32 || fmt.Sprint(c.allow) != "[map[cidr:203.0.113.7/32] map[cidr:2001:db8::/32 description:v6]]" {
							return fmt.Errorf("shop: %+v", c)
						}
						return nil
					}),
				),
			},
			{
				// Back to sleeping when idle: the node's default timeout.
				Config: config(`always_on = false
  max_in_flight = 32`, "elasticsearch"),
				Check: resource.ComposeAggregateTestCheckFunc(
					resource.TestCheckResourceAttr("roll_search_cluster.shop", "idle_timeout_s", "900"),
					resource.TestCheckResourceAttr("roll_search_cluster.shop", "ip_allowlist.#", "0"),
				),
			},
			{
				// A new flavor switches the cluster in place, keeping its data; its key doesn't carry
				// over, so it's made again on it.
				Config: config(`max_in_flight = 32`, "opensearch"),
				ConfigPlanChecks: resource.ConfigPlanChecks{PreApply: []plancheck.PlanCheck{
					plancheck.ExpectResourceAction("roll_search_cluster.logs", plancheck.ResourceActionUpdate),
					plancheck.ExpectResourceAction("roll_search_api_key.shipper", plancheck.ResourceActionDestroyBeforeCreate),
					plancheck.ExpectResourceAction("roll_search_cluster.shop", plancheck.ResourceActionNoop),
				}},
				Check: resource.ComposeAggregateTestCheckFunc(
					resource.TestCheckResourceAttr("roll_search_cluster.logs", "version", "3.9.0"),
					resource.TestCheckResourceAttr("roll_search_api_key.shipper", "flavor", "opensearch"),
					resource.TestCheckResourceAttr("roll_search_api_key.shipper", "username", "roll-key-key1@logs"),
					resource.TestCheckResourceAttr("roll_search_api_key.shipper", "password", "secret-roll-key-key1"),
					resource.TestCheckNoResourceAttr("roll_search_api_key.shipper", "encoded"),
					func(s *terraform.State) error {
						f.mu.Lock()
						defer f.mu.Unlock()
						id := s.RootModule().Resources["roll_search_cluster.logs"].Primary.ID
						if c := f.byIDOrName("logs"); id != logsID || c == nil || c.id != logsID || c.flavor != "opensearch" || len(f.deleted) != 0 {
							return fmt.Errorf("logs is %s (was %s), deleted %v", id, logsID, f.deleted)
						}
						return nil
					},
				),
			},
			{ResourceName: "roll_search_cluster.shop", ImportState: true, ImportStateVerify: true, ImportStateVerifyIgnore: []string{"password", "status"}},
			{ResourceName: "roll_search_cluster.shop", ImportState: true, ImportStateId: "shop", ImportStateVerify: true, ImportStateVerifyIgnore: []string{"password", "status"}},
			{ResourceName: "roll_search_snapshot_repository.backups", ImportState: true, ImportStateVerify: true, ImportStateVerifyIgnore: []string{"credentials"}, ImportStateIdFunc: idOf("roll_search_snapshot_repository.backups")},
			{
				// Revoked elsewhere, on a loaded cluster: the next apply makes it again.
				PreConfig: func() {
					f.mu.Lock()
					defer f.mu.Unlock()
					f.clusters[shopID].keys["key1"] = true
				},
				Config: config(`max_in_flight = 32`, "opensearch"),
				Check:  resource.TestCheckResourceAttr("roll_search_api_key.reader", "id", "key2"),
			},
		},
		CheckDestroy: func(*terraform.State) error {
			f.mu.Lock()
			defer f.mu.Unlock()
			if len(f.clusters) != 0 || len(f.sealed) != 0 {
				return fmt.Errorf("left behind: %v %v", f.clusters, f.sealed)
			}
			if !contains(f.revoked, "key2") || !contains(f.revoked, "roll-key-key1") {
				return fmt.Errorf("not revoked: %v", f.revoked)
			}
			return nil
		},
	})
}

// Past a person's cap the create's 409 is a diagnostic that names the cap and the flag.
func TestSearchClusterCap(t *testing.T) {
	f := newFakeSearch()
	f.capClusters, f.capAlwaysOn = 1, 1
	f.clusters["srch_old"] = &fakeCluster{id: "srch_old", name: "old", flavor: "elasticsearch", idle: 0}
	srv := httptest.NewServer(f)
	defer srv.Close()
	resource.UnitTest(t, resource.TestCase{
		ProtoV6ProviderFactories: searchFactories(),
		Steps: []resource.TestStep{{
			Config: fmt.Sprintf(`
provider "roll" {
  api_url = %q
  token   = "rlt_terraform"
}
resource "roll_search_cluster" "more" {
  name = "more"
}
`, srv.URL),
			ExpectError: regexp.MustCompile(`(?s)past your roll search cap.*a person may have 1.*--search-max-clusters-per-person`),
		}},
	})
}

// An ephemeral key for the run: a provisioner uses it, it's never in the state, and it's revoked
// as the run closes.
func TestSearchKeyEphemeral(t *testing.T) {
	f := newFakeSearch()
	f.clusters["srch_1"] = &fakeCluster{id: "srch_1", name: "shop", flavor: "elasticsearch", idle: 900, maxInFlight: 64, maxBody: 100, allow: []map[string]any{}, keys: map[string]bool{}}
	srv := httptest.NewServer(f)
	defer srv.Close()
	used := filepath.Join(t.TempDir(), "key")
	resource.UnitTest(t, resource.TestCase{
		ProtoV6ProviderFactories: searchFactories(),
		TerraformVersionChecks:   []tfversion.TerraformVersionCheck{tfversion.SkipBelow(tfversion.Version1_10_0)},
		Steps: []resource.TestStep{{
			Config: fmt.Sprintf(`
provider "roll" {
  api_url = %q
  token   = "rlt_terraform"
}
ephemeral "roll_search_api_key" "seed" {
  cluster    = "shop"
  name       = "seed"
  access     = "admin"
  expiration = "1h"
}
resource "terraform_data" "seed" {
  provisioner "local-exec" {
    command     = "printf %%s \"$KEY\" > %s"
    environment = { KEY = ephemeral.roll_search_api_key.seed.encoded }
  }
}
`, srv.URL, used),
			Check: func(s *terraform.State) error {
				f.mu.Lock()
				defer f.mu.Unlock()
				b, err := os.ReadFile(used)
				if err != nil || !regexp.MustCompile(`^key[0-9]+@shop:secret-key[0-9]+$`).MatchString(mustDecode(string(b))) {
					return fmt.Errorf("the provisioner didn't get the key (%v)", err)
				}
				if strings.Contains(fmt.Sprintf("%#v", s.RootModule().Resources), string(b)) {
					return fmt.Errorf("the key is in the state")
				}
				// Each open (plan and apply) made one; every one is revoked.
				for id, revoked := range f.clusters["srch_1"].keys {
					if !revoked {
						return fmt.Errorf("the run's key %s wasn't revoked", id)
					}
				}
				if len(f.revoked) == 0 {
					return fmt.Errorf("no key was made")
				}
				return nil
			},
		}},
	})
}

func mustDecode(s string) string {
	b, _ := base64.StdEncoding.DecodeString(s)
	return string(b)
}
