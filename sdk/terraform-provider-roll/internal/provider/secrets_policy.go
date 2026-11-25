package provider

import (
	"context"
	"net/http"

	"github.com/hashicorp/terraform-plugin-framework-validators/int64validator"
	"github.com/hashicorp/terraform-plugin-framework-validators/stringvalidator"
	"github.com/hashicorp/terraform-plugin-framework/attr"
	"github.com/hashicorp/terraform-plugin-framework/diag"
	"github.com/hashicorp/terraform-plugin-framework/path"
	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/booldefault"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/schema/validator"
	"github.com/hashicorp/terraform-plugin-framework/tfsdk"
	"github.com/hashicorp/terraform-plugin-framework/types"
)

// roll_secrets_policy: roll secrets' guardrails (docs/secrets.md, "Guardrails"), one per server.
type secretsPolicyModel struct {
	ID                  types.String `tfsdk:"id"`
	ProductionApprovals types.Int64  `tfsdk:"production_approvals"`
	LeaksRotate         types.String `tfsdk:"leaks_rotate"`
	SyncKinds           types.Set    `tfsdk:"sync_kinds"`
	NoStoredCloudKeys   types.Bool   `tfsdk:"no_stored_cloud_keys"`
	MaxTokenDays        types.Int64  `tfsdk:"max_token_days"`
}

const secretsPolicyPath = "/v1/secrets/policy"

type secretsPolicyResource struct{ c *client }

func newSecretsPolicyResource() resource.Resource { return &secretsPolicyResource{} }

func (r *secretsPolicyResource) Metadata(_ context.Context, req resource.MetadataRequest, resp *resource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_secrets_policy"
}

func (r *secretsPolicyResource) Schema(_ context.Context, _ resource.SchemaRequest, resp *resource.SchemaResponse) {
	resp.Schema = schema.Schema{
		Description: "roll secrets' policy: floors every project may only tighten (docs/secrets.md, \"Guardrails\"). The operator's token, or an admin account's. One per server owns the whole policy: a floor left out has none, and destroying it removes every floor. Nothing existing is rewritten; a new project's production and leaks.rotate are born at the floors, and Health lists each project's distance (off_policy).",
		Attributes: map[string]schema.Attribute{
			"id":                   schema.StringAttribute{Computed: true, Description: "Always policy.", PlanModifiers: []planmodifier.String{stringplanmodifier.UseStateForUnknown()}},
			"production_approvals": schema.Int64Attribute{Optional: true, Validators: []validator.Int64{int64validator.Between(0, 10)}, Description: "Every project's production stays protected with at least this many approvals."},
			"leaks_rotate":         schema.StringAttribute{Optional: true, Validators: []validator.String{stringvalidator.OneOf("managed")}, Description: "managed: every project's leaks.rotate stays managed."},
			"sync_kinds":           schema.SetAttribute{Optional: true, ElementType: types.StringType, Description: "The only sync kinds a project may make through roll secrets (empty: none)."},
			"no_stored_cloud_keys": schema.BoolAttribute{Optional: true, Computed: true, Default: booldefault.StaticBool(false), Description: "No sync stores a cloud key for AWS, Google or Azure where it could sign in without one (identities and workload identity instead)."},
			"max_token_days":       schema.Int64Attribute{Optional: true, Validators: []validator.Int64{int64validator.Between(1, 3650)}, Description: "The longest a roll token with a secrets rule may live, in days."},
		},
	}
}

func (r *secretsPolicyResource) Configure(_ context.Context, req resource.ConfigureRequest, _ *resource.ConfigureResponse) {
	if c, ok := req.ProviderData.(*client); ok {
		r.c = c
	}
}

func (m *secretsPolicyModel) body(ctx context.Context, d *diag.Diagnostics) map[string]any {
	b := map[string]any{"no_stored_cloud_keys": m.NoStoredCloudKeys.ValueBool()}
	if !m.ProductionApprovals.IsNull() {
		b["production_approvals"] = m.ProductionApprovals.ValueInt64()
	}
	if !m.LeaksRotate.IsNull() {
		b["leaks_rotate"] = m.LeaksRotate.ValueString()
	}
	if !m.SyncKinds.IsNull() {
		kinds := []string{}
		d.Append(m.SyncKinds.ElementsAs(ctx, &kinds, false)...)
		b["sync_kinds"] = kinds
	}
	if !m.MaxTokenDays.IsNull() {
		b["max_token_days"] = m.MaxTokenDays.ValueInt64()
	}
	return b
}

func (m *secretsPolicyModel) fill(v map[string]any) {
	m.ID = types.StringValue("policy")
	m.ProductionApprovals = jInt(v, "production_approvals")
	m.LeaksRotate = jStr(v, "leaks_rotate")
	m.NoStoredCloudKeys = jBool(v, "no_stored_cloud_keys")
	m.MaxTokenDays = jInt(v, "max_token_days")
	if xs, ok := v["sync_kinds"].([]any); ok {
		kinds := make([]attr.Value, 0, len(xs))
		for _, x := range xs {
			if s, ok := x.(string); ok {
				kinds = append(kinds, types.StringValue(s))
			}
		}
		m.SyncKinds = types.SetValueMust(types.StringType, kinds)
	} else {
		m.SyncKinds = types.SetNull(types.StringType)
	}
}

func (r *secretsPolicyResource) write(ctx context.Context, plan tfsdk.Plan, state *tfsdk.State, d *diag.Diagnostics) {
	var m secretsPolicyModel
	d.Append(plan.Get(ctx, &m)...)
	body := m.body(ctx, d)
	if d.HasError() {
		return
	}
	var v map[string]any
	if err := r.c.call(ctx, http.MethodPut, secretsPolicyPath, body, &v); err != nil {
		d.AddError("setting roll secrets' policy", err.Error())
		return
	}
	m.fill(v)
	d.Append(state.Set(ctx, m)...)
}

func (r *secretsPolicyResource) Create(ctx context.Context, req resource.CreateRequest, resp *resource.CreateResponse) {
	r.write(ctx, req.Plan, &resp.State, &resp.Diagnostics)
}

func (r *secretsPolicyResource) Update(ctx context.Context, req resource.UpdateRequest, resp *resource.UpdateResponse) {
	r.write(ctx, req.Plan, &resp.State, &resp.Diagnostics)
}

func (r *secretsPolicyResource) Read(ctx context.Context, _ resource.ReadRequest, resp *resource.ReadResponse) {
	var v map[string]any
	if err := r.c.call(ctx, http.MethodGet, secretsPolicyPath, nil, &v); err != nil {
		resp.Diagnostics.AddError("reading roll secrets' policy", err.Error())
		return
	}
	var m secretsPolicyModel
	m.fill(v)
	resp.Diagnostics.Append(resp.State.Set(ctx, m)...)
}

func (r *secretsPolicyResource) Delete(ctx context.Context, _ resource.DeleteRequest, resp *resource.DeleteResponse) {
	if err := r.c.call(ctx, http.MethodPut, secretsPolicyPath, map[string]any{}, nil); err != nil {
		resp.Diagnostics.AddError("removing roll secrets' policy", err.Error())
	}
}

func (r *secretsPolicyResource) ImportState(ctx context.Context, _ resource.ImportStateRequest, resp *resource.ImportStateResponse) {
	resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root("id"), "policy")...)
}
