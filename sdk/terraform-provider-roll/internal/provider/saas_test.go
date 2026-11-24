package provider

import (
	"fmt"
	"net/http/httptest"
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

// Obviously fake vendor credentials.
const (
	fakeGitlabAdmin  = "fake-gitlab-admin-credential-1"
	fakeGitlabAdmin2 = "fake-gitlab-admin-credential-2"
)

// A SaaS rotator as code: registered with a write-only credential, the credential replaced in place
// (a PATCH, so linked keys keep working) with its end date, another target replacing it, imported;
// the credential never in the plan or the state.
func TestSaasRotatorAsCode(t *testing.T) {
	f := &fakeRolld{envs: map[string]*environment{}, keys: map[string]map[string]any{}, identities: map[string]map[string]any{}, syncs: map[string]map[string]any{}, tokens: map[string]map[string]any{}}
	srv := httptest.NewServer(f)
	defer srv.Close()
	hcl := func(name, project string, version int, expires string) string {
		return fmt.Sprintf(`
provider "roll" {
  api_url = %q
  token   = "rlt_terraform"
  project = "prj_a"
}
variable "gitlab_admin" {
  type      = string
  sensitive = true
  ephemeral = true
}
resource "roll_secrets_saas_rotator" "gitlab" {
  name                  = %q
  vendor                = "gitlab"
  target                = jsonencode({ host = "GitLab.Example.com", project = %q, scopes = ["read_api"] })
  credential_wo         = var.gitlab_admin
  credential_wo_version = %d
  credential_expires_at = %q
}
`, srv.URL, name, project, version, expires)
	}
	credentials := noneOf{fakeGitlabAdmin, fakeGitlabAdmin2}
	notKept := func(s *terraform.State) error {
		state := fmt.Sprintf("%#v", s.RootModule().Resources)
		if strings.Contains(state, fakeGitlabAdmin) || strings.Contains(state, fakeGitlabAdmin2) {
			return fmt.Errorf("a credential is in the state")
		}
		return nil
	}
	sent := func(want string) resource.TestCheckFunc {
		return func(*terraform.State) error {
			f.mu.Lock()
			defer f.mu.Unlock()
			if len(f.received) == 0 || f.received[len(f.received)-1] != want {
				return fmt.Errorf("roll was last sent %v", f.received)
			}
			return nil
		}
	}
	vars := func(cred string) config.Variables {
		return config.Variables{"gitlab_admin": config.StringVariable(cred)}
	}
	resource.UnitTest(t, resource.TestCase{
		ProtoV6ProviderFactories: map[string]func() (tfprotov6.ProviderServer, error){"roll": providerserver.NewProtocol6WithError(New("test")())},
		TerraformVersionChecks:   []tfversion.TerraformVersionCheck{tfversion.SkipBelow(tfversion.Version1_11_0)},
		Steps: []resource.TestStep{
			{
				Config:           hcl("gitlab", "acme/web", 1, "2030-01-01T00:00:00Z"),
				ConfigVariables:  vars(fakeGitlabAdmin),
				ConfigPlanChecks: resource.ConfigPlanChecks{PreApply: []plancheck.PlanCheck{credentials}},
				Check: resource.ComposeAggregateTestCheckFunc(
					resource.TestCheckResourceAttr("roll_secrets_saas_rotator.gitlab", "id", "saa_1"),
					resource.TestCheckNoResourceAttr("roll_secrets_saas_rotator.gitlab", "credential_wo"),
					resource.TestCheckResourceAttr("roll_secrets_saas_rotator.gitlab", "credential_expires_at", "2030-01-01T00:00:00Z"),
					sent(fakeGitlabAdmin),
					notKept,
				),
			},
			{
				// roll's defaults and its lowercased host aren't a change.
				Config:           hcl("gitlab", "acme/web", 1, "2030-01-01T00:00:00Z"),
				ConfigVariables:  vars(fakeGitlabAdmin),
				ConfigPlanChecks: resource.ConfigPlanChecks{PreApply: []plancheck.PlanCheck{plancheck.ExpectEmptyPlan()}},
			},
			{
				// A new credential, its end and a new name: in place, the registration kept.
				Config:           hcl("gitlab-ops", "acme/web", 2, "2031-06-30T00:00:00Z"),
				ConfigVariables:  vars(fakeGitlabAdmin2),
				ConfigPlanChecks: resource.ConfigPlanChecks{PreApply: []plancheck.PlanCheck{credentials, plancheck.ExpectResourceAction("roll_secrets_saas_rotator.gitlab", plancheck.ResourceActionUpdate)}},
				Check: resource.ComposeAggregateTestCheckFunc(
					resource.TestCheckResourceAttr("roll_secrets_saas_rotator.gitlab", "id", "saa_1"),
					resource.TestCheckResourceAttr("roll_secrets_saas_rotator.gitlab", "name", "gitlab-ops"),
					sent(fakeGitlabAdmin2),
					notKept,
					func(*terraform.State) error {
						f.mu.Lock()
						defer f.mu.Unlock()
						r := f.regs["saas"]["saa_1"]
						if len(f.regs["saas"]) != 1 || r["credential_expires_at"] != float64(1940544000000) || r["credential"] != nil {
							return fmt.Errorf("not replaced in place: %v", f.regs["saas"])
						}
						return nil
					},
				),
			},
			{
				// Another target is another registration.
				Config:           hcl("gitlab-ops", "acme/api", 2, "2031-06-30T00:00:00Z"),
				ConfigVariables:  vars(fakeGitlabAdmin2),
				ConfigPlanChecks: resource.ConfigPlanChecks{PreApply: []plancheck.PlanCheck{credentials, plancheck.ExpectResourceAction("roll_secrets_saas_rotator.gitlab", plancheck.ResourceActionDestroyBeforeCreate)}},
				Check: resource.ComposeAggregateTestCheckFunc(notKept, func(*terraform.State) error {
					f.mu.Lock()
					defer f.mu.Unlock()
					for _, r := range f.regs["saas"] {
						if len(f.regs["saas"]) == 1 && r["target"].(map[string]any)["project"] == "acme/api" {
							return nil
						}
					}
					return fmt.Errorf("not registered again: %v", f.regs["saas"])
				}),
			},
			// The credential never comes back; the target comes back with roll's defaults filled in.
			{ResourceName: "roll_secrets_saas_rotator.gitlab", ImportState: true, ImportStateVerify: true, ImportStateVerifyIgnore: []string{"credential_wo_version", "target"}, ConfigVariables: vars(fakeGitlabAdmin2)},
		},
		CheckDestroy: func(*terraform.State) error {
			f.mu.Lock()
			defer f.mu.Unlock()
			if len(f.regs["saas"]) != 0 {
				return fmt.Errorf("left behind: %v", f.regs["saas"])
			}
			return nil
		},
	})
}

// A folder's change policy, and an environment's approvals and approvers, as code: approvers the
// dashboard set stay when Terraform doesn't name them.
func TestFolderPolicyAndApprovalsAsCode(t *testing.T) {
	f := &fakeRolld{envs: map[string]*environment{}, keys: map[string]map[string]any{}, identities: map[string]map[string]any{}, syncs: map[string]map[string]any{}, tokens: map[string]map[string]any{}}
	srv := httptest.NewServer(f)
	defer srv.Close()
	hcl := func(approvals int, approver string) string {
		return fmt.Sprintf(`
provider "roll" {
  api_url = %q
  token   = "rlt_terraform"
  project = "prj_a"
}
resource "roll_secrets_environment" "staging" {
  slug      = "staging"
  protected = true
  approvals = %[2]d
  approvers = [%[3]q]
}
resource "roll_secrets_environment" "production" {
  slug      = "production"
  protected = true
}
resource "roll_secrets_folder_policy" "payments" {
  environment = roll_secrets_environment.staging.slug
  path        = "/payments"
  approvals   = %[2]d
  approvers   = [%[3]q]
}
`, srv.URL, approvals, approver)
	}
	// Environment policy writes so far.
	policies := func() int { return len(f.policies) }
	var before int
	resource.UnitTest(t, resource.TestCase{
		ProtoV6ProviderFactories: map[string]func() (tfprotov6.ProviderServer, error){"roll": providerserver.NewProtocol6WithError(New("test")())},
		Steps: []resource.TestStep{
			{
				Config: hcl(2, "role:owner"),
				Check: resource.ComposeAggregateTestCheckFunc(
					resource.TestCheckResourceAttr("roll_secrets_environment.staging", "approvals", "2"),
					resource.TestCheckResourceAttr("roll_secrets_environment.staging", "approvers.0", "role:owner"),
					resource.TestCheckResourceAttr("roll_secrets_environment.production", "approvals", "1"),
					resource.TestCheckResourceAttr("roll_secrets_folder_policy.payments", "id", "prj_a/staging/payments"),
					resource.TestCheckResourceAttr("roll_secrets_folder_policy.payments", "approvals", "2"),
					func(*terraform.State) error {
						f.mu.Lock()
						defer f.mu.Unlock()
						if p := f.folders["staging:/payments"]; p == nil || fmt.Sprint(p["approvers"]) != "[role:owner]" {
							return fmt.Errorf("folder policies: %v", f.folders)
						}
						return nil
					},
				),
			},
			{
				// Someone adds an approver to production in the dashboard: Terraform doesn't name
				// approvers there, so they stay; staging's and the folder's change in place.
				PreConfig: func() {
					f.mu.Lock()
					defer f.mu.Unlock()
					f.envs["production"].Approvers = []string{"usr_fake_dashboard"}
					before = policies()
				},
				Config: hcl(3, "role:admin"),
				ConfigPlanChecks: resource.ConfigPlanChecks{PreApply: []plancheck.PlanCheck{
					plancheck.ExpectResourceAction("roll_secrets_folder_policy.payments", plancheck.ResourceActionUpdate),
					plancheck.ExpectResourceAction("roll_secrets_environment.staging", plancheck.ResourceActionUpdate),
					plancheck.ExpectResourceAction("roll_secrets_environment.production", plancheck.ResourceActionNoop),
				}},
				Check: resource.ComposeAggregateTestCheckFunc(
					resource.TestCheckResourceAttr("roll_secrets_environment.staging", "approvals", "3"),
					resource.TestCheckResourceAttr("roll_secrets_environment.production", "approvers.0", "usr_fake_dashboard"),
					resource.TestCheckResourceAttr("roll_secrets_folder_policy.payments", "approvals", "3"),
					resource.TestCheckResourceAttr("roll_secrets_folder_policy.payments", "approvers.0", "role:admin"),
					func(*terraform.State) error {
						f.mu.Lock()
						defer f.mu.Unlock()
						if got := policies(); got != before+1 {
							return fmt.Errorf("one policy write (staging's), got %d", got-before)
						}
						return nil
					},
				),
			},
			{ResourceName: "roll_secrets_folder_policy.payments", ImportState: true, ImportStateId: "prj_a/staging/payments", ImportStateVerify: true},
		},
		CheckDestroy: func(*terraform.State) error {
			f.mu.Lock()
			defer f.mu.Unlock()
			if len(f.folders) != 0 {
				return fmt.Errorf("left behind: %v", f.folders)
			}
			return nil
		},
	})
}
