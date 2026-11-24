package provider

import (
	"context"
	"encoding/json"
	"strings"
	"time"

	"github.com/hashicorp/terraform-plugin-framework-validators/stringvalidator"
	"github.com/hashicorp/terraform-plugin-framework/diag"
	"github.com/hashicorp/terraform-plugin-framework/path"
	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/schema/validator"
	"github.com/hashicorp/terraform-plugin-framework/types"
)

// roll_secrets_saas_rotator: a vendor credential roll issues and revokes the vendor's own keys with
// (docs/secrets.md, "SaaS rotators"). The credential is write-only; replacing it is a PATCH, so the
// keys linked to the registration keep working.
type saasRotatorModel struct {
	ID                  types.String `tfsdk:"id"`
	Project             types.String `tfsdk:"project"`
	Name                types.String `tfsdk:"name"`
	Vendor              types.String `tfsdk:"vendor"`
	Target              types.String `tfsdk:"target"`
	CredentialWO        types.String `tfsdk:"credential_wo"`
	CredentialWOVersion types.Int64  `tfsdk:"credential_wo_version"`
	CredentialExpiresAt types.String `tfsdk:"credential_expires_at"`
}

var saasVendors = []string{"sendgrid", "twilio", "okta", "entra", "auth0", "gitlab", "openai", "datadog", "cloudflare", "databricks", "confluent"}

// rfc3339ms is an RFC 3339 time as unix ms (ok false for anything else).
func rfc3339ms(s types.String) (int64, bool) {
	t, err := time.Parse(time.RFC3339, s.ValueString())
	return t.UnixMilli(), err == nil
}

// msOrNil is the time to send: ms, or null when it isn't set.
func msOrNil(s types.String) any {
	if s.IsNull() || s.IsUnknown() {
		return nil
	}
	ms, _ := rfc3339ms(s)
	return ms
}

func fillSaasRotator(m *saasRotatorModel, v map[string]any) {
	m.ID, m.Name, m.Vendor, m.CredentialWO = jStr(v, "id"), jStr(v, "name"), jStr(v, "vendor"), types.StringNull()
	target, _ := v["target"].(map[string]any)
	if target == nil {
		target = map[string]any{}
	}
	if !sameTarget(m.Target, target) {
		b, _ := json.Marshal(target)
		m.Target = types.StringValue(string(b))
	}
	switch at := jInt(v, "credential_expires_at"); {
	case at.IsNull():
		m.CredentialExpiresAt = types.StringNull()
	case func() bool { ms, ok := rfc3339ms(m.CredentialExpiresAt); return ok && ms == at.ValueInt64() }():
		// As configured: the same time, however it was written.
	default:
		m.CredentialExpiresAt = types.StringValue(time.UnixMilli(at.ValueInt64()).UTC().Format(time.RFC3339))
	}
}

// sameTarget: what's configured is what roll keeps, before roll filled in its defaults (GitLab's
// host and access level, Datadog's site…) and lowercased its hosts.
func sameTarget(configured types.String, kept map[string]any) bool {
	if configured.IsNull() || configured.IsUnknown() {
		return false
	}
	var want map[string]any
	if json.Unmarshal([]byte(configured.ValueString()), &want) != nil {
		return false
	}
	return jsonWithin(want, kept)
}

func jsonWithin(want, have any) bool {
	switch w := want.(type) {
	case map[string]any:
		h, ok := have.(map[string]any)
		if !ok {
			return false
		}
		for k, x := range w {
			if !jsonWithin(x, h[k]) {
				return false
			}
		}
		return true
	case []any:
		h, ok := have.([]any)
		if !ok || len(h) != len(w) {
			return false
		}
		for i := range w {
			if !jsonWithin(w[i], h[i]) {
				return false
			}
		}
		return true
	case string:
		h, ok := have.(string)
		return ok && strings.EqualFold(strings.TrimSpace(w), h)
	default:
		return want == have
	}
}

// keepTarget plans the state's target when the configuration says the same thing (roll's defaults
// and lowercasing aside), so an import or a normalised target doesn't replace the registration.
type keepTarget struct{}

func (keepTarget) Description(context.Context) string {
	return "kept from the state while the configuration names the same target"
}
func (k keepTarget) MarkdownDescription(ctx context.Context) string { return k.Description(ctx) }

func (keepTarget) PlanModifyString(_ context.Context, req planmodifier.StringRequest, resp *planmodifier.StringResponse) {
	if req.StateValue.IsNull() || req.StateValue.IsUnknown() || req.ConfigValue.IsUnknown() {
		return
	}
	var kept map[string]any
	if json.Unmarshal([]byte(req.StateValue.ValueString()), &kept) != nil {
		return
	}
	configured := req.ConfigValue
	if configured.IsNull() {
		configured = types.StringValue("{}")
	}
	if sameTarget(configured, kept) {
		resp.PlanValue = req.StateValue
	}
}

func newSaasRotatorResource() resource.Resource {
	return &registration[saasRotatorModel]{
		typeName: "_secrets_saas_rotator", route: "saas", what: "SaaS rotator",
		description: "A SaaS rotator (docs/secrets.md, \"SaaS rotators\"): a vendor credential roll issues and revokes the vendor's own keys with (SendGrid, Twilio, Okta, Entra, Auth0, GitLab, OpenAI, Datadog, Cloudflare, Databricks, Confluent). Link keys to it (`kind = \"saas\"`, field `secret` or `id`, `mode = \"managed\"`). The credential is write-only (Terraform 1.11+): checked with the vendor, sealed in roll, never in the plan or the state. A new credential (bump credential_wo_version), the name and credential_expires_at change in place, and the linked keys keep working; another vendor or target replaces it.",
		schema: map[string]schema.Attribute{
			"name": inPlace("Its name: letters, digits, - and _."),
			"vendor": schema.StringAttribute{Required: true, Description: "sendgrid, twilio, okta, entra, auth0, gitlab, openai, datadog, cloudflare, databricks or confluent.",
				Validators:    []validator.String{stringvalidator.OneOf(saasVendors...)},
				PlanModifiers: []planmodifier.String{stringplanmodifier.RequiresReplace()}},
			"target": schema.StringAttribute{Optional: true, Computed: true,
				Description:   "The vendor's account or app, as JSON (jsonencode({ project = \"acme/web\", scopes = [\"read_api\"] }); the table in docs/secrets.md, \"SaaS rotators\"). None: {} (SendGrid with full access). roll fills in defaults (GitLab's host, Datadog's site…), which don't count as a change; another target replaces the registration.",
				PlanModifiers: []planmodifier.String{keepTarget{}, stringplanmodifier.RequiresReplace(), stringplanmodifier.UseStateForUnknown()}},
			"credential_wo": schema.StringAttribute{Required: true, Sensitive: true, WriteOnly: true,
				Description: "The vendor credential roll issues keys with (the table in docs/secrets.md: an API key, an Auth Token, a client secret, `<API key>:<application key>`, `KEY:SECRET`…). Write-only: checked with the vendor (read only) before roll keeps it, sealed there, never in the plan or the state. Change credential_wo_version to send a new one in place: a credential the vendor refuses leaves the old one."},
			"credential_wo_version": schema.Int64Attribute{Required: true, Description: "Bump it to replace the credential with credential_wo (in place: linked keys keep working, and their next rotation uses the new one)."},
			"credential_expires_at": schema.StringAttribute{Optional: true,
				Description: "When the credential ends at the vendor (RFC 3339, in the future): health warns two weeks before. A new credential without it has no end date."},
		},
		keys: func(m *saasRotatorModel) (*types.String, *types.String) { return &m.Project, &m.ID },
		validate: func(_ context.Context, m *saasRotatorModel, d *diag.Diagnostics) {
			if !m.Target.IsNull() && !m.Target.IsUnknown() {
				var t map[string]any
				if json.Unmarshal([]byte(m.Target.ValueString()), &t) != nil {
					d.AddAttributeError(path.Root("target"), "target is a JSON object", "write it with jsonencode({ … }).")
				}
			}
			if !m.CredentialExpiresAt.IsNull() && !m.CredentialExpiresAt.IsUnknown() {
				if _, ok := rfc3339ms(m.CredentialExpiresAt); !ok {
					d.AddAttributeError(path.Root("credential_expires_at"), "credential_expires_at", "an RFC 3339 time, e.g. 2027-03-31T00:00:00Z")
				}
			}
		},
		body: func(_ context.Context, m *saasRotatorModel, cfg func(string) types.String, _ *diag.Diagnostics) map[string]any {
			target := map[string]any{}
			if !m.Target.IsNull() && !m.Target.IsUnknown() {
				_ = json.Unmarshal([]byte(m.Target.ValueString()), &target)
			}
			b := map[string]any{"name": m.Name.ValueString(), "vendor": m.Vendor.ValueString(), "target": target, "credential": cfg("credential_wo").ValueString()}
			if at := msOrNil(m.CredentialExpiresAt); at != nil {
				b["credential_expires_at"] = at
			}
			return b
		},
		fill: fillSaasRotator,
		update: func(ctx context.Context, c *client, at string, plan, prior *saasRotatorModel, cfg func(string) types.String, d *diag.Diagnostics) {
			body := map[string]any{}
			changed(body, "name", plan.Name, prior.Name)
			if !plan.CredentialWOVersion.Equal(prior.CredentialWOVersion) {
				// The date goes with the credential: roll drops the old one's otherwise.
				body["credential"], body["credential_expires_at"] = cfg("credential_wo").ValueString(), msOrNil(plan.CredentialExpiresAt)
			} else if !plan.CredentialExpiresAt.Equal(prior.CredentialExpiresAt) {
				pm, pok := rfc3339ms(plan.CredentialExpiresAt)
				was, wok := rfc3339ms(prior.CredentialExpiresAt)
				if pok != wok || pm != was {
					body["credential_expires_at"] = msOrNil(plan.CredentialExpiresAt)
				}
			}
			plan.CredentialWO = types.StringNull()
			patch(ctx, c, at, body, plan, fillSaasRotator, "SaaS rotator", d)
		},
	}
}
