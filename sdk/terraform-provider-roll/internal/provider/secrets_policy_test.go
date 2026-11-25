package provider

import (
	"fmt"
	"net/http/httptest"
	"testing"

	"github.com/hashicorp/terraform-plugin-framework/providerserver"
	"github.com/hashicorp/terraform-plugin-go/tfprotov6"
	"github.com/hashicorp/terraform-plugin-testing/helper/resource"
	"github.com/hashicorp/terraform-plugin-testing/terraform"
)

func TestSecretsPolicyAsCode(t *testing.T) {
	f := &fakeRolld{envs: map[string]*environment{}, keys: map[string]map[string]any{}, identities: map[string]map[string]any{}, syncs: map[string]map[string]any{}, tokens: map[string]map[string]any{}}
	srv := httptest.NewServer(f)
	defer srv.Close()
	hcl := func(floors string) string {
		return fmt.Sprintf(`
provider "roll" {
  api_url = %q
  token   = "rlt_terraform"
}
resource "roll_secrets_policy" "floors" {
%s
}
`, srv.URL, floors)
	}
	sent := func(check func(map[string]any) error) resource.TestCheckFunc {
		return func(*terraform.State) error {
			f.mu.Lock()
			defer f.mu.Unlock()
			return check(f.guardrails)
		}
	}
	resource.UnitTest(t, resource.TestCase{
		ProtoV6ProviderFactories: map[string]func() (tfprotov6.ProviderServer, error){"roll": providerserver.NewProtocol6WithError(New("test")())},
		Steps: []resource.TestStep{
			{
				Config: hcl(`  production_approvals = 1
  leaks_rotate         = "managed"
  sync_kinds           = ["vercel", "github"]`),
				Check: resource.ComposeAggregateTestCheckFunc(
					resource.TestCheckResourceAttr("roll_secrets_policy.floors", "id", "policy"),
					resource.TestCheckResourceAttr("roll_secrets_policy.floors", "production_approvals", "1"),
					resource.TestCheckResourceAttr("roll_secrets_policy.floors", "sync_kinds.#", "2"),
					resource.TestCheckResourceAttr("roll_secrets_policy.floors", "no_stored_cloud_keys", "false"),
					resource.TestCheckNoResourceAttr("roll_secrets_policy.floors", "max_token_days"),
					sent(func(g map[string]any) error {
						if g["production_approvals"] != float64(1) || g["leaks_rotate"] != "managed" || fmt.Sprint(g["sync_kinds"]) != "[github vercel]" {
							return fmt.Errorf("policy sent: %v", g)
						}
						return nil
					}),
				),
			},
			{
				// The whole policy each time: a floor left out of the config is left out of roll's.
				Config: hcl(`  production_approvals = 2
  no_stored_cloud_keys = true
  max_token_days       = 30`),
				Check: resource.ComposeAggregateTestCheckFunc(
					resource.TestCheckResourceAttr("roll_secrets_policy.floors", "max_token_days", "30"),
					resource.TestCheckNoResourceAttr("roll_secrets_policy.floors", "sync_kinds"),
					sent(func(g map[string]any) error {
						if _, ok := g["sync_kinds"]; ok || g["leaks_rotate"] != nil || g["no_stored_cloud_keys"] != true {
							return fmt.Errorf("policy sent: %v", g)
						}
						return nil
					}),
				),
			},
			{ResourceName: "roll_secrets_policy.floors", ImportState: true, ImportStateId: "policy", ImportStateVerify: true},
		},
		CheckDestroy: func(*terraform.State) error {
			f.mu.Lock()
			defer f.mu.Unlock()
			if len(f.guardrails) != 0 {
				return fmt.Errorf("floors left behind: %v", f.guardrails)
			}
			return nil
		},
	})
}
