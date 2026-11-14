package provider

import (
	"context"
	"net/http"
	"net/url"

	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/listplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/mapplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringdefault"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/types"
)

// roll_secrets_sync: an environment's folder pushed to a platform (GitHub Actions secrets,
// Vercel, AWS Parameter Store…; docs/secrets.md, "Syncs") whenever it changes. roll has no way to
// change one in place, so every attribute replaces it.
type syncResource struct{ c *client }

type syncModel struct {
	ID            types.String `tfsdk:"id"`
	Project       types.String `tfsdk:"project"`
	Environment   types.String `tfsdk:"environment"`
	Path          types.String `tfsdk:"path"`
	Kind          types.String `tfsdk:"kind"`
	Target        types.Map    `tfsdk:"target"`
	Keys          types.List   `tfsdk:"keys"`
	Prefix        types.String `tfsdk:"prefix"`
	SigningSecret types.String `tfsdk:"signing_secret"`
}

func newSyncResource() resource.Resource { return &syncResource{} }

func (r *syncResource) Metadata(_ context.Context, req resource.MetadataRequest, resp *resource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_secrets_sync"
}

func (r *syncResource) Schema(_ context.Context, _ resource.SchemaRequest, resp *resource.SchemaResponse) {
	replace := []planmodifier.String{stringplanmodifier.RequiresReplace()}
	resp.Schema = schema.Schema{
		Description: "A folder of an environment pushed to a platform whenever it changes. The target names the platform's place and the key that holds its credential (`token_key`), never the credential itself. roll lists a sync's target by host only, so a target changed outside Terraform isn't seen.",
		Attributes: map[string]schema.Attribute{
			"id":          schema.StringAttribute{Computed: true, PlanModifiers: []planmodifier.String{stringplanmodifier.UseStateForUnknown()}},
			"project":     schema.StringAttribute{Optional: true, Computed: true, PlanModifiers: append(replace, stringplanmodifier.UseStateForUnknown())},
			"environment": schema.StringAttribute{Required: true, PlanModifiers: replace},
			"path":        schema.StringAttribute{Optional: true, Computed: true, Default: stringdefault.StaticString("/"), PlanModifiers: replace},
			"kind":        schema.StringAttribute{Required: true, PlanModifiers: replace, Description: "github, gitlab, vercel, aws_ssm, kubernetes, webhook…"},
			"target": schema.MapAttribute{Required: true, ElementType: types.StringType, Description: "The kind's target fields (`GET …/syncs` lists them per kind).",
				PlanModifiers: []planmodifier.Map{mapplanmodifier.RequiresReplace()}},
			"keys": schema.ListAttribute{Optional: true, ElementType: types.StringType, Description: "Only these keys (all when left out).",
				PlanModifiers: []planmodifier.List{listplanmodifier.RequiresReplace()}},
			"prefix":         schema.StringAttribute{Optional: true, Computed: true, Default: stringdefault.StaticString(""), PlanModifiers: replace, Description: "Only keys starting with it."},
			"signing_secret": schema.StringAttribute{Computed: true, Sensitive: true, PlanModifiers: []planmodifier.String{stringplanmodifier.UseStateForUnknown()}, Description: "A signed kind's (webhook) signing secret, shown once when it's made."},
		},
	}
}

func (r *syncResource) Configure(_ context.Context, req resource.ConfigureRequest, _ *resource.ConfigureResponse) {
	if c, ok := req.ProviderData.(*client); ok {
		r.c = c
	}
}

func (r *syncResource) base(m *syncModel) string {
	return "/v1/secrets/projects/" + url.PathEscape(m.Project.ValueString()) + "/syncs"
}

func (r *syncResource) Create(ctx context.Context, req resource.CreateRequest, resp *resource.CreateResponse) {
	var m syncModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &m)...)
	p, ok := project(r.c, m.Project)
	if !ok {
		resp.Diagnostics.AddError("which project?", "set project here or on the provider (ROLL_PROJECT)")
	}
	if resp.Diagnostics.HasError() {
		return
	}
	m.Project = types.StringValue(p)
	target := map[string]string{}
	resp.Diagnostics.Append(m.Target.ElementsAs(ctx, &target, false)...)
	body := map[string]any{"environment": m.Environment.ValueString(), "path": m.Path.ValueString(), "kind": m.Kind.ValueString(), "target": target, "prefix": m.Prefix.ValueString()}
	if keys := stringList(ctx, m.Keys, &resp.Diagnostics); keys != nil {
		body["keys"] = keys
	}
	if resp.Diagnostics.HasError() {
		return
	}
	var made struct {
		ID            string `json:"id"`
		SigningSecret string `json:"signing_secret"`
	}
	if err := r.c.call(ctx, http.MethodPost, r.base(&m), body, &made); err != nil {
		resp.Diagnostics.AddError("creating the sync", err.Error())
		return
	}
	m.ID, m.SigningSecret = types.StringValue(made.ID), types.StringValue(made.SigningSecret)
	resp.Diagnostics.Append(resp.State.Set(ctx, m)...)
}

func (r *syncResource) Read(ctx context.Context, req resource.ReadRequest, resp *resource.ReadResponse) {
	var m syncModel
	resp.Diagnostics.Append(req.State.Get(ctx, &m)...)
	if resp.Diagnostics.HasError() {
		return
	}
	var all struct {
		Syncs []struct {
			ID string `json:"id"`
		} `json:"syncs"`
	}
	if err := r.c.call(ctx, http.MethodGet, r.base(&m), nil, &all); err != nil {
		resp.Diagnostics.AddError("reading the sync", err.Error())
		return
	}
	for _, s := range all.Syncs {
		if s.ID == m.ID.ValueString() {
			return // as configured: its target is listed by host only
		}
	}
	resp.State.RemoveResource(ctx)
}

func (r *syncResource) Update(_ context.Context, _ resource.UpdateRequest, resp *resource.UpdateResponse) {
	resp.Diagnostics.AddError("syncs don't change in place", "every attribute replaces the sync")
}

func (r *syncResource) Delete(ctx context.Context, req resource.DeleteRequest, resp *resource.DeleteResponse) {
	var m syncModel
	resp.Diagnostics.Append(req.State.Get(ctx, &m)...)
	if resp.Diagnostics.HasError() {
		return
	}
	if err := r.c.call(ctx, http.MethodDelete, r.base(&m)+"/"+url.PathEscape(m.ID.ValueString()), nil, nil); err != nil && !notFound(err) {
		resp.Diagnostics.AddError("deleting the sync", err.Error())
	}
}
