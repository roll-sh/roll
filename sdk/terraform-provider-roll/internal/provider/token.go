package provider

import (
	"context"
	"net/http"
	"net/url"
	"time"

	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/listplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/types"
)

// roll_secrets_token: a token for CI or a service with one rule on a project's secrets, a roll
// token (roll-access's grant, docs/permissions.md) made through POST /v1/access/tokens like the
// dashboard's New token. Every attribute replaces it (a new secret); destroying it revokes it.
type tokenResource struct{ c *client }

type tokenModel struct {
	ID           types.String `tfsdk:"id"`
	Project      types.String `tfsdk:"project"`
	Name         types.String `tfsdk:"name"`
	Environments types.List   `tfsdk:"environments"`
	Paths        types.List   `tfsdk:"paths"`
	Actions      types.List   `tfsdk:"actions"`
	ExpiresAt    types.String `tfsdk:"expires_at"`
	AllowedIPs   types.List   `tfsdk:"allowed_ips"`
	Token        types.String `tfsdk:"token"`
}

func newTokenResource() resource.Resource { return &tokenResource{} }

func (r *tokenResource) Metadata(_ context.Context, req resource.MetadataRequest, resp *resource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_secrets_token"
}

func (r *tokenResource) Schema(_ context.Context, _ resource.SchemaRequest, resp *resource.SchemaResponse) {
	replace := []planmodifier.String{stringplanmodifier.RequiresReplace()}
	list := func(required bool, desc string) schema.ListAttribute {
		return schema.ListAttribute{Required: required, Optional: !required, ElementType: types.StringType, Description: desc, PlanModifiers: []planmodifier.List{listplanmodifier.RequiresReplace()}}
	}
	resp.Schema = schema.Schema{
		Description: "A roll token for CI or a service with one rule on a project's secrets. Its secret is in the state (sensitive): keep the state as private as the token. Destroying it revokes it at once.",
		Attributes: map[string]schema.Attribute{
			"id":           schema.StringAttribute{Computed: true, PlanModifiers: []planmodifier.String{stringplanmodifier.UseStateForUnknown()}},
			"project":      schema.StringAttribute{Optional: true, Computed: true, PlanModifiers: append(replace, stringplanmodifier.UseStateForUnknown())},
			"name":         schema.StringAttribute{Required: true, PlanModifiers: replace},
			"environments": list(false, "The environments it reaches (all when left out)."),
			"paths":        list(false, "The folders it reaches (all when left out)."),
			"actions":      list(true, "read, reveal, write, delete, admin."),
			"expires_at":   schema.StringAttribute{Optional: true, PlanModifiers: replace, Description: "When it stops working (RFC 3339); never when left out."},
			"allowed_ips":  list(false, "Client addresses or CIDRs it works from (any when left out)."),
			"token":        schema.StringAttribute{Computed: true, Sensitive: true, PlanModifiers: []planmodifier.String{stringplanmodifier.UseStateForUnknown()}, Description: "The token (rlt_…), shown once when it's made."},
		},
	}
}

func (r *tokenResource) Configure(_ context.Context, req resource.ConfigureRequest, _ *resource.ConfigureResponse) {
	if c, ok := req.ProviderData.(*client); ok {
		r.c = c
	}
}

func (r *tokenResource) Create(ctx context.Context, req resource.CreateRequest, resp *resource.CreateResponse) {
	var m tokenModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &m)...)
	p, ok := project(r.c, m.Project)
	if !ok {
		resp.Diagnostics.AddError("which project?", "set project here or on the provider (ROLL_PROJECT)")
	}
	if resp.Diagnostics.HasError() {
		return
	}
	m.Project = types.StringValue(p)
	actions := map[string]string{}
	for _, a := range stringList(ctx, m.Actions, &resp.Diagnostics) {
		actions[a] = "allow"
	}
	rule := map[string]any{"product": "secrets", "resources": []map[string]string{{"project": p}}, "actions": actions}
	if envs := stringList(ctx, m.Environments, &resp.Diagnostics); envs != nil {
		rule["environments"] = envs
	}
	if paths := stringList(ctx, m.Paths, &resp.Diagnostics); paths != nil {
		rule["paths"] = paths
	}
	body := map[string]any{"name": m.Name.ValueString(), "rules": []any{rule}}
	if !m.ExpiresAt.IsNull() && !m.ExpiresAt.IsUnknown() {
		at, err := time.Parse(time.RFC3339, m.ExpiresAt.ValueString())
		if err != nil {
			resp.Diagnostics.AddError("expires_at", "an RFC 3339 time, e.g. 2027-01-01T00:00:00Z")
			return
		}
		body["expires_at"] = at.UnixMilli()
	}
	if ips := stringList(ctx, m.AllowedIPs, &resp.Diagnostics); ips != nil {
		body["ip_allow"] = ips
	}
	if resp.Diagnostics.HasError() {
		return
	}
	var made struct {
		Grant struct {
			ID string `json:"id"`
		} `json:"grant"`
		Token string `json:"token"`
	}
	if err := r.c.call(ctx, http.MethodPost, "/v1/access/tokens", body, &made); err != nil {
		resp.Diagnostics.AddError("creating the token", err.Error())
		return
	}
	m.ID, m.Token = types.StringValue(made.Grant.ID), types.StringValue(made.Token)
	resp.Diagnostics.Append(resp.State.Set(ctx, m)...)
}

func (r *tokenResource) Read(ctx context.Context, req resource.ReadRequest, resp *resource.ReadResponse) {
	var m tokenModel
	resp.Diagnostics.Append(req.State.Get(ctx, &m)...)
	if resp.Diagnostics.HasError() {
		return
	}
	var g struct {
		RevokedAt *int64 `json:"revoked_at"`
		ExpiresAt *int64 `json:"expires_at"`
	}
	err := r.c.call(ctx, http.MethodGet, "/v1/access/tokens/"+url.PathEscape(m.ID.ValueString()), nil, &g)
	gone := notFound(err) || (err == nil && (g.RevokedAt != nil || (g.ExpiresAt != nil && *g.ExpiresAt <= time.Now().UnixMilli())))
	if gone {
		resp.State.RemoveResource(ctx)
		return
	}
	if err != nil {
		resp.Diagnostics.AddError("reading the token", err.Error())
	}
}

func (r *tokenResource) Update(_ context.Context, _ resource.UpdateRequest, resp *resource.UpdateResponse) {
	resp.Diagnostics.AddError("tokens don't change in place", "every attribute replaces the token")
}

func (r *tokenResource) Delete(ctx context.Context, req resource.DeleteRequest, resp *resource.DeleteResponse) {
	var m tokenModel
	resp.Diagnostics.Append(req.State.Get(ctx, &m)...)
	if resp.Diagnostics.HasError() {
		return
	}
	if err := r.c.call(ctx, http.MethodDelete, "/v1/access/tokens/"+url.PathEscape(m.ID.ValueString()), nil, nil); err != nil && !notFound(err) {
		resp.Diagnostics.AddError("revoking the token", err.Error())
	}
}
