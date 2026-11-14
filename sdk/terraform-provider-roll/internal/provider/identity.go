package provider

import (
	"context"
	"net/http"
	"net/url"

	"github.com/hashicorp/terraform-plugin-framework/attr"
	"github.com/hashicorp/terraform-plugin-framework/diag"
	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/int64planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/listplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/mapplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/types"
)

// roll_secrets_identity: a workload identity (docs/secrets.md, "Workload identities"): CI whose
// OIDC tokens match logs in for a short-lived token with this scope. roll has no way to change one
// in place, so every attribute replaces it (its live tokens end with it).
type identityResource struct{ c *client }

type identityModel struct {
	ID           types.String `tfsdk:"id"`
	Project      types.String `tfsdk:"project"`
	Name         types.String `tfsdk:"name"`
	Issuer       types.String `tfsdk:"issuer"`
	Audience     types.String `tfsdk:"audience"`
	Subject      types.String `tfsdk:"subject"`
	Claims       types.Map    `tfsdk:"claims"`
	Environments types.List   `tfsdk:"environments"`
	Paths        types.List   `tfsdk:"paths"`
	Actions      types.List   `tfsdk:"actions"`
	TTLMinutes   types.Int64  `tfsdk:"ttl_minutes"`
	AllowedIPs   types.List   `tfsdk:"allowed_ips"`
}

func newIdentityResource() resource.Resource { return &identityResource{} }

func (r *identityResource) Metadata(_ context.Context, req resource.MetadataRequest, resp *resource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_secrets_identity"
}

func (r *identityResource) Schema(_ context.Context, _ resource.SchemaRequest, resp *resource.SchemaResponse) {
	str := func(required bool, desc string) schema.StringAttribute {
		return schema.StringAttribute{Required: required, Optional: !required, Description: desc, PlanModifiers: []planmodifier.String{stringplanmodifier.RequiresReplace()}}
	}
	list := func(required bool, desc string) schema.ListAttribute {
		return schema.ListAttribute{Required: required, Optional: !required, ElementType: types.StringType, Description: desc, PlanModifiers: []planmodifier.List{listplanmodifier.RequiresReplace()}}
	}
	resp.Schema = schema.Schema{
		Description: "A workload identity: CI or a cluster logs in with its own OIDC token for a short-lived token, so nothing long-lived sits in its settings. roll makes identities for people and the operator only, never for a token: use the operator's token or a person's.",
		Attributes: map[string]schema.Attribute{
			"id":       schema.StringAttribute{Computed: true, PlanModifiers: []planmodifier.String{stringplanmodifier.UseStateForUnknown()}},
			"project":  schema.StringAttribute{Optional: true, Computed: true, PlanModifiers: []planmodifier.String{stringplanmodifier.RequiresReplace(), stringplanmodifier.UseStateForUnknown()}},
			"name":     str(true, "Its name."),
			"issuer":   str(true, "The OIDC token's iss, e.g. https://token.actions.githubusercontent.com."),
			"audience": str(true, "The token's aud: the API's URL, as the CLI and the GitHub Action ask for."),
			"subject":  str(true, "The token's sub, or a pattern of it with * (repo:acme/web:ref:refs/heads/main). Shared issuers must name one repository."),
			"claims": schema.MapAttribute{Optional: true, ElementType: types.StringType, Description: "Other claims that must match (ref, environment…).",
				PlanModifiers: []planmodifier.Map{mapplanmodifier.RequiresReplace()}},
			"environments": list(false, "The environments a login reaches (all when left out)."),
			"paths":        list(false, "The folders a login reaches (all when left out)."),
			"actions":      list(true, "read, reveal, write, delete, admin."),
			"ttl_minutes": schema.Int64Attribute{Optional: true, Computed: true, Description: "How long a login's token lasts, 5–60 (default 15).",
				PlanModifiers: []planmodifier.Int64{int64planmodifier.RequiresReplace(), int64planmodifier.UseStateForUnknown()}},
			"allowed_ips": list(false, "Client addresses or CIDRs it may log in from (any when left out)."),
		},
	}
}

func (r *identityResource) Configure(_ context.Context, req resource.ConfigureRequest, _ *resource.ConfigureResponse) {
	if c, ok := req.ProviderData.(*client); ok {
		r.c = c
	}
}

type identityJSON struct {
	ID           string            `json:"id,omitempty"`
	Name         string            `json:"name"`
	Issuer       string            `json:"issuer"`
	Audience     string            `json:"audience"`
	Subject      string            `json:"subject"`
	Claims       map[string]string `json:"claims,omitempty"`
	Environments []string          `json:"environments,omitempty"`
	Paths        []string          `json:"paths,omitempty"`
	Actions      []string          `json:"actions"`
	TTLMinutes   int64             `json:"ttl_minutes,omitempty"`
	AllowedIPs   []string          `json:"allowed_ips,omitempty"`
}

func stringList(ctx context.Context, l types.List, d *diag.Diagnostics) []string {
	if l.IsNull() || l.IsUnknown() {
		return nil
	}
	var out []string
	d.Append(l.ElementsAs(ctx, &out, false)...)
	return out
}

func listOf(xs []string, keepNull types.List) types.List {
	if len(xs) == 0 && keepNull.IsNull() {
		return types.ListNull(types.StringType)
	}
	vs := make([]attr.Value, len(xs))
	for i, x := range xs {
		vs[i] = types.StringValue(x)
	}
	return types.ListValueMust(types.StringType, vs)
}

func (r *identityResource) base(m *identityModel) string {
	return "/v1/secrets/projects/" + url.PathEscape(m.Project.ValueString()) + "/identities"
}

func (r *identityResource) Create(ctx context.Context, req resource.CreateRequest, resp *resource.CreateResponse) {
	var m identityModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &m)...)
	p, ok := project(r.c, m.Project)
	if !ok {
		resp.Diagnostics.AddError("which project?", "set project here or on the provider (ROLL_PROJECT)")
	}
	if resp.Diagnostics.HasError() {
		return
	}
	m.Project = types.StringValue(p)
	n := identityJSON{Name: m.Name.ValueString(), Issuer: m.Issuer.ValueString(), Audience: m.Audience.ValueString(), Subject: m.Subject.ValueString(),
		Environments: stringList(ctx, m.Environments, &resp.Diagnostics), Paths: stringList(ctx, m.Paths, &resp.Diagnostics), Actions: stringList(ctx, m.Actions, &resp.Diagnostics),
		AllowedIPs: stringList(ctx, m.AllowedIPs, &resp.Diagnostics)}
	if !m.Claims.IsNull() && !m.Claims.IsUnknown() {
		resp.Diagnostics.Append(m.Claims.ElementsAs(ctx, &n.Claims, false)...)
	}
	if !m.TTLMinutes.IsNull() && !m.TTLMinutes.IsUnknown() {
		n.TTLMinutes = m.TTLMinutes.ValueInt64()
	}
	var made identityJSON
	if err := r.c.call(ctx, http.MethodPost, r.base(&m), n, &made); err != nil {
		resp.Diagnostics.AddError("creating the identity", err.Error())
		return
	}
	m.ID, m.TTLMinutes = types.StringValue(made.ID), types.Int64Value(made.TTLMinutes)
	resp.Diagnostics.Append(resp.State.Set(ctx, m)...)
}

func (r *identityResource) Read(ctx context.Context, req resource.ReadRequest, resp *resource.ReadResponse) {
	var m identityModel
	resp.Diagnostics.Append(req.State.Get(ctx, &m)...)
	if resp.Diagnostics.HasError() {
		return
	}
	var all []identityJSON
	if err := r.c.call(ctx, http.MethodGet, r.base(&m), nil, &all); err != nil {
		resp.Diagnostics.AddError("reading the identity", err.Error())
		return
	}
	for _, i := range all {
		if i.ID != m.ID.ValueString() {
			continue
		}
		m.Name, m.Issuer, m.Audience, m.Subject, m.TTLMinutes = types.StringValue(i.Name), types.StringValue(i.Issuer), types.StringValue(i.Audience), types.StringValue(i.Subject), types.Int64Value(i.TTLMinutes)
		m.Environments, m.Paths, m.Actions, m.AllowedIPs = listOf(i.Environments, m.Environments), listOf(i.Paths, m.Paths), listOf(i.Actions, m.Actions), listOf(i.AllowedIPs, m.AllowedIPs)
		resp.Diagnostics.Append(resp.State.Set(ctx, m)...)
		return
	}
	resp.State.RemoveResource(ctx)
}

func (r *identityResource) Update(_ context.Context, _ resource.UpdateRequest, resp *resource.UpdateResponse) {
	resp.Diagnostics.AddError("identities don't change in place", "every attribute replaces the identity")
}

func (r *identityResource) Delete(ctx context.Context, req resource.DeleteRequest, resp *resource.DeleteResponse) {
	var m identityModel
	resp.Diagnostics.Append(req.State.Get(ctx, &m)...)
	if resp.Diagnostics.HasError() {
		return
	}
	if err := r.c.call(ctx, http.MethodDelete, r.base(&m)+"/"+url.PathEscape(m.ID.ValueString()), nil, nil); err != nil && !notFound(err) {
		resp.Diagnostics.AddError("deleting the identity", err.Error())
	}
}
