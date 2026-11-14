package provider

import (
	"context"
	"net/http"
	"net/url"

	"github.com/hashicorp/terraform-plugin-framework/path"
	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/booldefault"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/types"
)

// roll_secrets_environment: one of a project's standard environments (`PUT …/environments/{env}`
// ensures it), its name and whether it's protected (changes to it go through approved requests).
type environmentResource struct{ c *client }

type environmentModel struct {
	ID        types.String `tfsdk:"id"`
	Project   types.String `tfsdk:"project"`
	Slug      types.String `tfsdk:"slug"`
	Name      types.String `tfsdk:"name"`
	Protected types.Bool   `tfsdk:"protected"`
}

func newEnvironmentResource() resource.Resource { return &environmentResource{} }

func (r *environmentResource) Metadata(_ context.Context, req resource.MetadataRequest, resp *resource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_secrets_environment"
}

func (r *environmentResource) Schema(_ context.Context, _ resource.SchemaRequest, resp *resource.SchemaResponse) {
	replace := []planmodifier.String{stringplanmodifier.RequiresReplace()}
	resp.Schema = schema.Schema{
		Description: "An environment of a project's secrets. Deleting a protected one deletes it on purpose (Terraform passes its confirmation).",
		Attributes: map[string]schema.Attribute{
			"id":        schema.StringAttribute{Computed: true, PlanModifiers: []planmodifier.String{stringplanmodifier.UseStateForUnknown()}},
			"project":   schema.StringAttribute{Optional: true, Computed: true, PlanModifiers: append(replace, stringplanmodifier.UseStateForUnknown()), Description: "prj_…; the provider's project when left out."},
			"slug":      schema.StringAttribute{Required: true, PlanModifiers: replace, Description: "Its slug (production, staging, qa…)."},
			"name":      schema.StringAttribute{Optional: true, Computed: true, PlanModifiers: []planmodifier.String{stringplanmodifier.UseStateForUnknown()}, Description: "Its display name."},
			"protected": schema.BoolAttribute{Optional: true, Computed: true, Default: booldefault.StaticBool(false), Description: "Writes go through change requests someone else approves."},
		},
	}
}

func (r *environmentResource) Configure(_ context.Context, req resource.ConfigureRequest, _ *resource.ConfigureResponse) {
	if c, ok := req.ProviderData.(*client); ok {
		r.c = c
	}
}

func (r *environmentResource) apply(ctx context.Context, m *environmentModel) error {
	p, _ := project(r.c, m.Project)
	m.Project = types.StringValue(p)
	body := map[string]any{}
	if !m.Name.IsNull() && !m.Name.IsUnknown() {
		body["name"] = m.Name.ValueString()
	}
	if err := r.c.call(ctx, http.MethodPut, r.c.envPath(p, m.Slug.ValueString()), body, nil); err != nil {
		return err
	}
	// The name of one that existed already is changed in place (ensure keeps it).
	if name, ok := body["name"]; ok {
		if err := r.c.call(ctx, http.MethodPatch, r.c.envPath(p, m.Slug.ValueString()), map[string]any{"name": name}, nil); err != nil {
			return err
		}
	}
	// Only when it changes: the policy also holds approvers and a notify URL set elsewhere.
	want := m.Protected.ValueBool()
	if err := r.read(ctx, m); err != nil {
		return err
	}
	if m.Protected.ValueBool() != want {
		if err := r.c.call(ctx, http.MethodPut, r.c.envPath(p, m.Slug.ValueString())+"/policy", map[string]any{"protected": want}, nil); err != nil {
			return err
		}
		return r.read(ctx, m)
	}
	return nil
}

func (r *environmentResource) read(ctx context.Context, m *environmentModel) error {
	p, _ := project(r.c, m.Project)
	var e environment
	if err := r.c.call(ctx, http.MethodGet, r.c.envPath(p, m.Slug.ValueString()), nil, &e); err != nil {
		return err
	}
	m.ID = types.StringValue(p + "/" + e.Slug)
	m.Project, m.Name, m.Protected = types.StringValue(p), types.StringValue(e.Name), types.BoolValue(e.Protected)
	return nil
}

func (r *environmentResource) Create(ctx context.Context, req resource.CreateRequest, resp *resource.CreateResponse) {
	var m environmentModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &m)...)
	if _, ok := project(r.c, m.Project); !ok {
		resp.Diagnostics.AddAttributeError(path.Root("project"), "which project?", "set project here or on the provider (ROLL_PROJECT)")
	}
	if resp.Diagnostics.HasError() {
		return
	}
	if err := r.apply(ctx, &m); err != nil {
		resp.Diagnostics.AddError("creating the environment", err.Error())
		return
	}
	resp.Diagnostics.Append(resp.State.Set(ctx, m)...)
}

func (r *environmentResource) Read(ctx context.Context, req resource.ReadRequest, resp *resource.ReadResponse) {
	var m environmentModel
	resp.Diagnostics.Append(req.State.Get(ctx, &m)...)
	if resp.Diagnostics.HasError() {
		return
	}
	if err := r.read(ctx, &m); notFound(err) {
		resp.State.RemoveResource(ctx)
		return
	} else if err != nil {
		resp.Diagnostics.AddError("reading the environment", err.Error())
		return
	}
	resp.Diagnostics.Append(resp.State.Set(ctx, m)...)
}

func (r *environmentResource) Update(ctx context.Context, req resource.UpdateRequest, resp *resource.UpdateResponse) {
	var m environmentModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &m)...)
	if resp.Diagnostics.HasError() {
		return
	}
	if err := r.apply(ctx, &m); err != nil {
		resp.Diagnostics.AddError("updating the environment", err.Error())
		return
	}
	resp.Diagnostics.Append(resp.State.Set(ctx, m)...)
}

func (r *environmentResource) Delete(ctx context.Context, req resource.DeleteRequest, resp *resource.DeleteResponse) {
	var m environmentModel
	resp.Diagnostics.Append(req.State.Get(ctx, &m)...)
	if resp.Diagnostics.HasError() {
		return
	}
	p, slug := m.Project.ValueString(), m.Slug.ValueString()
	err := r.c.call(ctx, http.MethodDelete, r.c.envPath(p, slug)+"?confirm="+url.QueryEscape(slug), nil, nil)
	if err != nil && !notFound(err) {
		resp.Diagnostics.AddError("deleting the environment", err.Error())
	}
}
