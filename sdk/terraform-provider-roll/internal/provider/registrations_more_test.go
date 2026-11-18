package provider

import (
	"context"
	"encoding/json"
	"fmt"
	"net/http/httptest"
	"regexp"
	"strings"
	"testing"

	"github.com/hashicorp/terraform-plugin-framework/providerserver"
	"github.com/hashicorp/terraform-plugin-go/tfprotov6"
	"github.com/hashicorp/terraform-plugin-testing/config"
	"github.com/hashicorp/terraform-plugin-testing/helper/resource"
	"github.com/hashicorp/terraform-plugin-testing/plancheck"
	"github.com/hashicorp/terraform-plugin-testing/terraform"
	"github.com/hashicorp/terraform-plugin-testing/tfversion"
)

// Fake credentials, obviously so (push protection scans for real-looking keys).
const (
	fakePrivateKey = "-----BEGIN FAKE KEY-----\nnot-a-key\n-----END FAKE KEY-----\n"
	fakeHECToken   = "fake-splunk-hec-token"
	fakeHookToken  = "fake-rotator-hook-token"
)

// noneOf fails a plan that holds any of the credentials.
type noneOf []string

func (n noneOf) CheckPlan(_ context.Context, req plancheck.CheckPlanRequest, resp *plancheck.CheckPlanResponse) {
	b, _ := json.Marshal(req.Plan)
	for _, s := range n {
		if strings.Contains(string(b), strings.SplitN(s, "\n", 2)[0]) {
			resp.Error = fmt.Errorf("the plan holds a credential (%.12s…)", s)
		}
	}
}

// A rotator, audit sinks, a GitHub App, an AWS role and IAM user, GCP and Azure accounts, the
// required keys and the KMS key, as code: made, changed in place where roll can, imported, destroyed,
// and no credential sent in (the App's private key, the SIEM's token, the token in the rotator's
// URL) in the plan or the state.
func TestMoreRegistrationsAsCode(t *testing.T) {
	f := &fakeRolld{envs: map[string]*environment{}, keys: map[string]map[string]any{}, identities: map[string]map[string]any{}, syncs: map[string]map[string]any{}, tokens: map[string]map[string]any{}}
	srv := httptest.NewServer(f)
	defer srv.Close()
	hcl := func(round int) string {
		required := `{ key = "DB_URL" }, { key = "PORT", path = "/api", type = "port", owner = "@payments" }`
		if round == 2 {
			required += `, { key = "SENTRY_DSN", comment = "errors" }`
		}
		return fmt.Sprintf(`
provider "roll" {
  api_url = %[1]q
  token   = "rlt_terraform"
  project = "prj_a"
}
# Credentials come in as ephemeral variables (or ephemeral resources): never in the plan.
variable "hec_token" {
  type      = string
  sensitive = true
  ephemeral = true
}
variable "app_key" {
  type      = string
  sensitive = true
  ephemeral = true
}
variable "hook_token" {
  type      = string
  sensitive = true
  ephemeral = true
}
resource "roll_secrets_rotator" "vendor" {
  name           = "vendor-%[2]d"
  url_wo         = "https://rotate%[2]d.example/hook?token=${var.hook_token}"
  url_wo_version = %[2]d
  rotate_secret  = { round = "%[2]d" }
}
resource "roll_secrets_audit_sink" "splunk" {
  url              = "https://splunk.example/services/collector"
  format           = "splunk"
  token_wo         = var.hec_token
  token_wo_version = 1
}
resource "roll_secrets_audit_sink" "search" {
  url = "search:logs/audit"
}
resource "roll_secrets_github_app" "deploy" {
  name                   = "deploy"
  app_id                 = 123
  installation_id        = 456
  private_key_wo         = var.app_key
  private_key_wo_version = 1
  repositories           = ["web"]
  permissions            = { contents = "write" }
}
resource "roll_secrets_aws_role" "deploy" {
  name     = "deploy"
  role_arn = "arn:aws:iam::123456789012:role/roll-%[2]d"
}
resource "roll_secrets_aws_user" "legacy" {
  name = "legacy-%[2]d"
  role = roll_secrets_aws_role.deploy.id
  user = "legacy-ci"
}
resource "roll_secrets_cloud_account" "gcp" {
  name            = "gcp"
  cloud           = "gcp"
  audience        = "//iam.googleapis.com/projects/1/locations/global/workloadIdentityPools/roll/providers/roll"
  service_account = "deploy@acme.iam.gserviceaccount.com"
}
resource "roll_secrets_cloud_account" "azure" {
  name      = "azure"
  cloud     = "azure"
  tenant    = "contoso.onmicrosoft.com"
  client_id = "00000000-0000-0000-0000-000000000000"
}
resource "roll_secrets_required_keys" "all" {
  keys = [%[3]s]
}
resource "roll_secrets_kms_key" "cmk" {
  key = "arn:aws:kms:eu-west-1:123456789012:key/fake-%[2]d"
}
`, srv.URL, round, required)
	}
	credentials := noneOf{fakePrivateKey, fakeHECToken, fakeHookToken}
	sentButNotKept := func(s *terraform.State) error {
		f.mu.Lock()
		defer f.mu.Unlock()
		got := strings.Join(f.received, "|")
		if !strings.Contains(got, fakePrivateKey) || !strings.Contains(got, fakeHECToken) || !strings.Contains(got, fakeHookToken) {
			return fmt.Errorf("roll was sent %q", got)
		}
		state := fmt.Sprintf("%#v", s.RootModule().Resources)
		if strings.Contains(state, "FAKE KEY") || strings.Contains(state, fakeHECToken) || strings.Contains(state, fakeHookToken) {
			return fmt.Errorf("a credential is in the state")
		}
		return nil
	}
	inPlace := func(names ...string) []plancheck.PlanCheck {
		checks := []plancheck.PlanCheck{credentials}
		for _, n := range names {
			checks = append(checks, plancheck.ExpectResourceAction(n, plancheck.ResourceActionUpdate))
		}
		return checks
	}
	vars := config.Variables{"hec_token": config.StringVariable(fakeHECToken), "app_key": config.StringVariable(fakePrivateKey), "hook_token": config.StringVariable(fakeHookToken)}
	imported := func(name string, ignore ...string) resource.TestStep {
		return resource.TestStep{ResourceName: name, ImportState: true, ImportStateVerify: true, ImportStateVerifyIgnore: ignore, ConfigVariables: vars}
	}
	resource.UnitTest(t, resource.TestCase{
		ProtoV6ProviderFactories: map[string]func() (tfprotov6.ProviderServer, error){"roll": providerserver.NewProtocol6WithError(New("test")())},
		TerraformVersionChecks:   []tfversion.TerraformVersionCheck{tfversion.SkipBelow(tfversion.Version1_11_0)},
		Steps: []resource.TestStep{
			{
				Config:           hcl(1),
				ConfigVariables:  vars,
				ConfigPlanChecks: resource.ConfigPlanChecks{PreApply: []plancheck.PlanCheck{credentials}},
				Check: resource.ComposeAggregateTestCheckFunc(
					resource.TestCheckResourceAttr("roll_secrets_rotator.vendor", "id", "rot_1"),
					resource.TestCheckResourceAttr("roll_secrets_rotator.vendor", "host", "rotate1.example"),
					resource.TestCheckResourceAttr("roll_secrets_rotator.vendor", "secret", "whsec_made_rot_1"),
					resource.TestCheckResourceAttr("roll_secrets_audit_sink.search", "format", "roll"),
					resource.TestCheckResourceAttrSet("roll_secrets_audit_sink.search", "secret"),
					resource.TestCheckNoResourceAttr("roll_secrets_audit_sink.splunk", "token_wo"),
					resource.TestCheckResourceAttr("roll_secrets_github_app.deploy", "api_base", "https://api.github.com"),
					resource.TestCheckNoResourceAttr("roll_secrets_github_app.deploy", "private_key_wo"),
					resource.TestCheckResourceAttr("roll_secrets_aws_role.deploy", "trust", "external_id"),
					resource.TestCheckResourceAttr("roll_secrets_cloud_account.azure", "scope", "https://management.azure.com/.default"),
					resource.TestCheckNoResourceAttr("roll_secrets_cloud_account.gcp", "scope"),
					resource.TestCheckResourceAttr("roll_secrets_required_keys.all", "keys.#", "2"),
					resource.TestCheckResourceAttr("roll_secrets_kms_key.cmk", "id", "prj_a"),
					sentButNotKept,
				),
			},
			{
				// Names, URLs, ARNs, the required list and the KMS key change in place; the
				// rotator's signing secret is rotated by its trigger.
				Config:          hcl(2),
				ConfigVariables: vars,
				ConfigPlanChecks: resource.ConfigPlanChecks{PreApply: inPlace("roll_secrets_rotator.vendor", "roll_secrets_aws_role.deploy", "roll_secrets_aws_user.legacy",
					"roll_secrets_required_keys.all", "roll_secrets_kms_key.cmk")},
				Check: resource.ComposeAggregateTestCheckFunc(
					resource.TestCheckResourceAttr("roll_secrets_rotator.vendor", "id", "rot_1"),
					resource.TestCheckResourceAttr("roll_secrets_rotator.vendor", "host", "rotate2.example"),
					resource.TestMatchResourceAttr("roll_secrets_rotator.vendor", "secret", regexp.MustCompile(`^whsec_rotated_`)),
					resource.TestCheckResourceAttr("roll_secrets_aws_user.legacy", "name", "legacy-2"),
					resource.TestCheckResourceAttr("roll_secrets_required_keys.all", "keys.#", "3"),
					func(*terraform.State) error {
						f.mu.Lock()
						defer f.mu.Unlock()
						role := f.regs["aws-roles"]["aws_1"]
						if len(f.regs["rotators"]) != 1 || len(f.regs["aws-roles"]) != 1 || role["role_arn"] != "arn:aws:iam::123456789012:role/roll-2" || f.kms != "arn:aws:kms:eu-west-1:123456789012:key/fake-2" {
							return fmt.Errorf("not changed in place: %v %v %q", f.regs["rotators"], f.regs["aws-roles"], f.kms)
						}
						return nil
					},
					sentButNotKept,
				),
			},
			// What roll shows once, or never, isn't imported.
			imported("roll_secrets_rotator.vendor", "url_wo_version", "secret", "rotate_secret"),
			imported("roll_secrets_audit_sink.splunk", "token_wo_version", "secret"),
			imported("roll_secrets_github_app.deploy", "private_key_wo_version"),
			imported("roll_secrets_aws_role.deploy"),
			imported("roll_secrets_aws_user.legacy"),
			imported("roll_secrets_cloud_account.gcp"),
			imported("roll_secrets_cloud_account.azure"),
			imported("roll_secrets_required_keys.all"),
			imported("roll_secrets_kms_key.cmk"),
		},
		CheckDestroy: func(*terraform.State) error {
			f.mu.Lock()
			defer f.mu.Unlock()
			for kind, items := range f.regs {
				if len(items) != 0 {
					return fmt.Errorf("%s left behind: %v", kind, items)
				}
			}
			if len(f.required) != 0 || f.kms != "" {
				return fmt.Errorf("required %v, kms %q", f.required, f.kms)
			}
			return nil
		},
	})
}
