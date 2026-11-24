package provider

import (
	"context"
	"net/http"
	"net/url"

	"github.com/hashicorp/terraform-plugin-framework-validators/int64validator"
	"github.com/hashicorp/terraform-plugin-framework-validators/stringvalidator"
	"github.com/hashicorp/terraform-plugin-framework/attr"
	"github.com/hashicorp/terraform-plugin-framework/diag"
	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/int64planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/schema/validator"
	"github.com/hashicorp/terraform-plugin-framework/types"
)

// roll_secrets_project: a project's own settings (`GET`/`PATCH /v1/secrets/projects/{project}`).
// The project itself is the workspace's; this resource neither makes nor deletes it.
type projectModel struct {
	ID             types.String `tfsdk:"id"`
	Project        types.String `tfsdk:"project"`
	LeakRotate     types.String `tfsdk:"leak_rotate"`
	HistoryDays    types.Int64  `tfsdk:"history_days"`
	ValidityChecks types.String `tfsdk:"validity_checks"`
}

type projectResource struct{ c *client }

func newProjectResource() resource.Resource { return &projectResource{} }

func (r *projectResource) Metadata(_ context.Context, req resource.MetadataRequest, resp *resource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_secrets_project"
}

func (r *projectResource) Schema(_ context.Context, _ resource.SchemaRequest, resp *resource.SchemaResponse) {
	keep := []planmodifier.String{stringplanmodifier.UseStateForUnknown()}
	resp.Schema = schema.Schema{
		Description: "A project's roll secrets settings (docs/secrets.md, \"Terraform\"). An attribute left out keeps roll's value; only what differs from roll's is written. Destroying it leaves the settings as they are, and the project stays. Imported by prj_….",
		Attributes: map[string]schema.Attribute{
			"id":      schema.StringAttribute{Computed: true, Description: "The project's id.", PlanModifiers: keep},
			"project": schema.StringAttribute{Optional: true, Computed: true, PlanModifiers: []planmodifier.String{stringplanmodifier.RequiresReplace(), stringplanmodifier.UseStateForUnknown()}, Description: "prj_…; the provider's project when left out."},
			"leak_rotate": schema.StringAttribute{Optional: true, Computed: true, PlanModifiers: keep, Validators: []validator.String{stringvalidator.OneOf("managed", "off")},
				Description: "What a leaked managed key does (\"When a value leaks\"): managed rotates it at once, with no grace; off (roll's default) reports it with a Rotate button."},
			"history_days": schema.Int64Attribute{Optional: true, Computed: true, PlanModifiers: []planmodifier.Int64{int64planmodifier.UseStateForUnknown()}, Validators: []validator.Int64{int64validator.Between(0, 3650)},
				Description: "How many days old values are kept (1–3650); 0 keeps them forever (roll's default)."},
			"validity_checks": schema.StringAttribute{Optional: true, Computed: true, PlanModifiers: keep, Validators: []validator.String{stringvalidator.OneOf("on", "off")},
				Description: "Whether credentials found in code are checked with their providers: on or off."},
		},
	}
}

func (r *projectResource) Configure(_ context.Context, req resource.ConfigureRequest, _ *resource.ConfigureResponse) {
	if c, ok := req.ProviderData.(*client); ok {
		r.c = c
	}
}

func projectAt(p string) string { return "/v1/secrets/projects/" + url.PathEscape(p) }

// fill sets m's settings from a GET answer.
func (m *projectModel) fill(v map[string]any) {
	leaks, _ := v["leaks"].(map[string]any)
	m.LeakRotate = jStr(leaks, "rotate")
	m.ValidityChecks = jStr(v, "validity_checks")
	m.HistoryDays = jInt(v, "history_days")
	if m.HistoryDays.IsNull() {
		m.HistoryDays = types.Int64Value(0)
	}
}

// write reads roll's settings, then PATCHes only those the plan sets to something else.
func (r *projectResource) write(ctx context.Context, plan projectModel, d *diag.Diagnostics) (projectModel, bool) {
	p, ok := project(r.c, plan.Project)
	if !ok {
		d.AddError("which project?", "set project here or on the provider (ROLL_PROJECT)")
		return plan, false
	}
	var now map[string]any
	if err := r.c.call(ctx, http.MethodGet, projectAt(p), nil, &now); err != nil {
		d.AddError("reading the project's settings", err.Error())
		return plan, false
	}
	var cur projectModel
	cur.fill(now)
	set := func(v attr.Value) bool { return !v.IsNull() && !v.IsUnknown() }
	body := map[string]any{}
	if set(plan.LeakRotate) && plan.LeakRotate != cur.LeakRotate {
		body["leaks"] = map[string]any{"rotate": plan.LeakRotate.ValueString()}
	}
	if set(plan.ValidityChecks) && plan.ValidityChecks != cur.ValidityChecks {
		body["validity_checks"] = plan.ValidityChecks.ValueString()
	}
	if set(plan.HistoryDays) && plan.HistoryDays != cur.HistoryDays {
		if n := plan.HistoryDays.ValueInt64(); n == 0 {
			body["history_days"] = nil
		} else {
			body["history_days"] = n
		}
	}
	if len(body) > 0 {
		if err := r.c.call(ctx, http.MethodPatch, projectAt(p), body, nil); err != nil {
			d.AddError("changing the project's settings", err.Error())
			return plan, false
		}
		if err := r.c.call(ctx, http.MethodGet, projectAt(p), nil, &now); err != nil {
			d.AddError("reading the project's settings", err.Error())
			return plan, false
		}
	}
	out := projectModel{ID: types.StringValue(p), Project: types.StringValue(p)}
	out.fill(now)
	return out, true
}

func (r *projectResource) Create(ctx context.Context, req resource.CreateRequest, resp *resource.CreateResponse) {
	var plan projectModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	if resp.Diagnostics.HasError() {
		return
	}
	if m, ok := r.write(ctx, plan, &resp.Diagnostics); ok {
		resp.Diagnostics.Append(resp.State.Set(ctx, m)...)
	}
}

func (r *projectResource) Update(ctx context.Context, req resource.UpdateRequest, resp *resource.UpdateResponse) {
	var plan projectModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	if resp.Diagnostics.HasError() {
		return
	}
	if m, ok := r.write(ctx, plan, &resp.Diagnostics); ok {
		resp.Diagnostics.Append(resp.State.Set(ctx, m)...)
	}
}

func (r *projectResource) Read(ctx context.Context, req resource.ReadRequest, resp *resource.ReadResponse) {
	var m projectModel
	resp.Diagnostics.Append(req.State.Get(ctx, &m)...)
	if resp.Diagnostics.HasError() {
		return
	}
	var v map[string]any
	if err := r.c.call(ctx, http.MethodGet, projectAt(m.Project.ValueString()), nil, &v); err != nil {
		if notFound(err) {
			resp.State.RemoveResource(ctx)
			return
		}
		resp.Diagnostics.AddError("reading the project's settings", err.Error())
		return
	}
	m.fill(v)
	resp.Diagnostics.Append(resp.State.Set(ctx, m)...)
}

// Delete leaves roll's settings as they are: the project and its settings aren't Terraform's to end.
func (r *projectResource) Delete(context.Context, resource.DeleteRequest, *resource.DeleteResponse) {}

func (r *projectResource) ImportState(ctx context.Context, req resource.ImportStateRequest, resp *resource.ImportStateResponse) {
	importProject(ctx, r.c, req, resp, "prj_…", false)
}
