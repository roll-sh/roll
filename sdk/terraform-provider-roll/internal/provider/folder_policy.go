package provider

import (
	"context"
	"net/http"
	"net/url"
	"strings"

	"github.com/hashicorp/terraform-plugin-framework-validators/int64validator"
	"github.com/hashicorp/terraform-plugin-framework/path"
	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/int64default"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/schema/validator"
	"github.com/hashicorp/terraform-plugin-framework/types"
)

// roll_secrets_folder_policy: a folder of an environment whose changes go through approved
// requests (docs/secrets.md, "Change policies per folder"), the folder and every folder under it.
type folderPolicyResource struct{ c *client }

type folderPolicyModel struct {
	ID          types.String `tfsdk:"id"`
	Project     types.String `tfsdk:"project"`
	Environment types.String `tfsdk:"environment"`
	Path        types.String `tfsdk:"path"`
	Approvals   types.Int64  `tfsdk:"approvals"`
	Approvers   types.List   `tfsdk:"approvers"`
}

type folderPolicy struct {
	Path      string   `json:"path"`
	Approvals int64    `json:"approvals"`
	Approvers []string `json:"approvers"`
}

var _ resource.ResourceWithImportState = (*folderPolicyResource)(nil)

func newFolderPolicyResource() resource.Resource { return &folderPolicyResource{} }

func (r *folderPolicyResource) Metadata(_ context.Context, req resource.MetadataRequest, resp *resource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_secrets_folder_policy"
}

func (r *folderPolicyResource) Schema(_ context.Context, _ resource.SchemaRequest, resp *resource.SchemaResponse) {
	replace := []planmodifier.String{stringplanmodifier.RequiresReplace()}
	resp.Schema = schema.Schema{
		Description: "A change policy on a folder (docs/secrets.md, \"Change policies per folder\"), as Infisical's approval policies on secret paths: in an environment that isn't protected as a whole, a write to a key in the folder or under it opens a change request that needs approvals. Imported by prj_…/<environment>/<path>.",
		Attributes: map[string]schema.Attribute{
			"id":          schema.StringAttribute{Computed: true, PlanModifiers: []planmodifier.String{stringplanmodifier.UseStateForUnknown()}},
			"project":     schema.StringAttribute{Optional: true, Computed: true, PlanModifiers: append(replace, stringplanmodifier.UseStateForUnknown()), Description: "prj_…; the provider's project when left out."},
			"environment": schema.StringAttribute{Required: true, PlanModifiers: replace, Description: "The environment's slug (not an ephemeral one)."},
			"path":        schema.StringAttribute{Required: true, PlanModifiers: replace, Description: "The folder (/payments); it covers every folder under it."},
			"approvals": schema.Int64Attribute{Optional: true, Computed: true, Default: int64default.StaticInt64(1), Description: "Approvals a request needs, from approvers other than its author: 1–10 (1 by default).",
				Validators: []validator.Int64{int64validator.Between(1, 10)}},
			"approvers": schema.ListAttribute{Optional: true, ElementType: types.StringType, Description: "Who may approve (principals, or role:owner, role:admin…); none: anyone with admin on the folder."},
		},
	}
}

func (r *folderPolicyResource) Configure(_ context.Context, req resource.ConfigureRequest, _ *resource.ConfigureResponse) {
	if c, ok := req.ProviderData.(*client); ok {
		r.c = c
	}
}

func (r *folderPolicyResource) at(m *folderPolicyModel) string {
	return r.c.envPath(m.Project.ValueString(), m.Environment.ValueString()) + "/folder-policies?path=" + url.QueryEscape(m.Path.ValueString())
}

func (r *folderPolicyResource) fill(m *folderPolicyModel, p folderPolicy) {
	m.ID = types.StringValue(m.Project.ValueString() + "/" + m.Environment.ValueString() + p.Path)
	// As configured when roll's is it normalised (no trailing /).
	if strings.TrimSuffix(m.Path.ValueString(), "/") != strings.TrimSuffix(p.Path, "/") || m.Path.IsNull() {
		m.Path = types.StringValue(p.Path)
	}
	m.Approvals, m.Approvers = types.Int64Value(p.Approvals), listOf(p.Approvers, m.Approvers)
}

func (r *folderPolicyResource) put(ctx context.Context, m *folderPolicyModel) error {
	body := map[string]any{"approvals": m.Approvals.ValueInt64(), "approvers": []string{}}
	var approvers []string
	if !m.Approvers.IsNull() && !m.Approvers.IsUnknown() {
		_ = m.Approvers.ElementsAs(ctx, &approvers, false)
		body["approvers"] = approvers
	}
	var p folderPolicy
	if err := r.c.call(ctx, http.MethodPut, r.at(m), body, &p); err != nil {
		return err
	}
	r.fill(m, p)
	return nil
}

func (r *folderPolicyResource) Create(ctx context.Context, req resource.CreateRequest, resp *resource.CreateResponse) {
	var m folderPolicyModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &m)...)
	p, ok := project(r.c, m.Project)
	if !ok {
		resp.Diagnostics.AddAttributeError(path.Root("project"), "which project?", "set project here or on the provider (ROLL_PROJECT)")
	}
	if resp.Diagnostics.HasError() {
		return
	}
	m.Project = types.StringValue(p)
	if err := r.put(ctx, &m); err != nil {
		resp.Diagnostics.AddError("creating the folder policy", err.Error())
		return
	}
	resp.Diagnostics.Append(resp.State.Set(ctx, m)...)
}

func (r *folderPolicyResource) Read(ctx context.Context, req resource.ReadRequest, resp *resource.ReadResponse) {
	var m folderPolicyModel
	resp.Diagnostics.Append(req.State.Get(ctx, &m)...)
	if resp.Diagnostics.HasError() {
		return
	}
	var all []folderPolicy
	err := r.c.call(ctx, http.MethodGet, r.c.envPath(m.Project.ValueString(), m.Environment.ValueString())+"/folder-policies", nil, &all)
	if notFound(err) {
		resp.State.RemoveResource(ctx)
		return
	} else if err != nil {
		resp.Diagnostics.AddError("reading the folder policy", err.Error())
		return
	}
	for _, p := range all {
		if strings.TrimSuffix(p.Path, "/") == strings.TrimSuffix(m.Path.ValueString(), "/") {
			r.fill(&m, p)
			resp.Diagnostics.Append(resp.State.Set(ctx, m)...)
			return
		}
	}
	resp.State.RemoveResource(ctx)
}

// Update puts the policy again: roll keeps one per folder.
func (r *folderPolicyResource) Update(ctx context.Context, req resource.UpdateRequest, resp *resource.UpdateResponse) {
	var m folderPolicyModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &m)...)
	if resp.Diagnostics.HasError() {
		return
	}
	if err := r.put(ctx, &m); err != nil {
		resp.Diagnostics.AddError("updating the folder policy", err.Error())
		return
	}
	resp.Diagnostics.Append(resp.State.Set(ctx, m)...)
}

func (r *folderPolicyResource) Delete(ctx context.Context, req resource.DeleteRequest, resp *resource.DeleteResponse) {
	var m folderPolicyModel
	resp.Diagnostics.Append(req.State.Get(ctx, &m)...)
	if resp.Diagnostics.HasError() {
		return
	}
	if err := r.c.call(ctx, http.MethodDelete, r.at(&m), nil, nil); err != nil && !notFound(err) {
		resp.Diagnostics.AddError("deleting the folder policy", err.Error())
	}
}

// ImportState takes prj_…/<environment>/<path> (the path starts with /: prj_a/staging/payments).
func (r *folderPolicyResource) ImportState(ctx context.Context, req resource.ImportStateRequest, resp *resource.ImportStateResponse) {
	parts := strings.SplitN(req.ID, "/", 3)
	if len(parts) != 3 || parts[0] == "" || parts[1] == "" {
		resp.Diagnostics.AddError("import id", req.ID+" isn't prj_…/<environment>/<path>")
		return
	}
	resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root("project"), parts[0])...)
	resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root("environment"), parts[1])...)
	resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root("path"), "/"+parts[2])...)
	resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root("id"), req.ID)...)
}
