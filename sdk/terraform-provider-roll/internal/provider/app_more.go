package provider

import (
	"context"
	"fmt"
	"net/http"
	"net/url"
	"strings"

	"github.com/hashicorp/terraform-plugin-framework/attr"
	"github.com/hashicorp/terraform-plugin-framework/datasource"
	dschema "github.com/hashicorp/terraform-plugin-framework/datasource/schema"
	"github.com/hashicorp/terraform-plugin-framework/diag"
	"github.com/hashicorp/terraform-plugin-framework/path"
	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/booldefault"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringdefault"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/types"
)

// roll apps' environments, links and notification targets as code, and a service to read.

func strings2list(s []string) types.List {
	v := make([]attr.Value, len(s))
	for i, x := range s {
		v[i] = types.StringValue(x)
	}
	return types.ListValueMust(types.StringType, v)
}

// list2strings is a list's strings, or nil when it's unset or not known yet.
func list2strings(ctx context.Context, l types.List, d *diag.Diagnostics) []string {
	if l.IsNull() || l.IsUnknown() {
		return nil
	}
	out := []string{}
	d.Append(l.ElementsAs(ctx, &out, false)...)
	return out
}

func configure(req resource.ConfigureRequest) *client {
	c, _ := req.ProviderData.(*client)
	return c
}

// ImportState for ids of `n` parts joined by "/", into `keys`.
func importParts(ctx context.Context, req resource.ImportStateRequest, resp *resource.ImportStateResponse, shape string, keys ...string) {
	parts := strings.SplitN(req.ID, "/", len(keys))
	if len(parts) != len(keys) {
		resp.Diagnostics.AddError("import id", fmt.Sprintf("%q isn't %s", req.ID, shape))
		return
	}
	for i, k := range keys {
		resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root(k), parts[i])...)
	}
}

// ── roll_app_environment ──

type appEnvironmentResource struct{ c *client }

type appEnvironmentModel struct {
	ID            types.String `tfsdk:"id"`
	Service       types.String `tfsdk:"service"`
	Name          types.String `tfsdk:"name"`
	Branch        types.String `tfsdk:"branch"`
	WaitForCI     types.Bool   `tfsdk:"wait_for_ci"`
	VariablesFrom types.String `tfsdk:"variables_from"`
	URL           types.String `tfsdk:"url"`
}

type appEnvironmentAnswer struct {
	Name      string  `json:"name"`
	Branch    *string `json:"branch"`
	WaitForCI bool    `json:"wait_for_ci"`
	URL       *string `json:"url"`
}

func newAppEnvironmentResource() resource.Resource { return &appEnvironmentResource{} }

func (r *appEnvironmentResource) Metadata(_ context.Context, req resource.MetadataRequest, resp *resource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_app_environment"
}

func (r *appEnvironmentResource) Schema(_ context.Context, _ resource.SchemaRequest, resp *resource.SchemaResponse) {
	replace := []planmodifier.String{stringplanmodifier.RequiresReplace()}
	resp.Schema = schema.Schema{
		Description: "Another environment of a service (staging, say), with its own variables, domains and URL. Production comes with the service.",
		Attributes: map[string]schema.Attribute{
			"id":             schema.StringAttribute{Computed: true, PlanModifiers: []planmodifier.String{stringplanmodifier.UseStateForUnknown()}},
			"service":        schema.StringAttribute{Required: true, PlanModifiers: replace},
			"name":           schema.StringAttribute{Required: true, PlanModifiers: replace, Description: "1–32 of a-z, 0-9 and -; not production."},
			"branch":         schema.StringAttribute{Optional: true, Description: "The branch whose pushes it deploys (a repo or Git URL service). None: it deploys only when asked."},
			"wait_for_ci":    schema.BoolAttribute{Optional: true, Computed: true, Default: booldefault.StaticBool(false), Description: "A roll code repo's push deploys once its checks pass."},
			"variables_from": schema.StringAttribute{Optional: true, PlanModifiers: replace, Description: "Start with another environment's variables (not its sealed ones). Read when it's made."},
			"url":            schema.StringAttribute{Computed: true, Description: "Its URL on the roll domain."},
		},
	}
}

func (r *appEnvironmentResource) Configure(_ context.Context, req resource.ConfigureRequest, _ *resource.ConfigureResponse) {
	r.c = configure(req)
}

func (m *appEnvironmentModel) fill(e *appEnvironmentAnswer) {
	m.ID = types.StringValue(m.Service.ValueString() + "/" + e.Name)
	m.Name = types.StringValue(e.Name)
	m.Branch = types.StringPointerValue(e.Branch)
	m.WaitForCI = types.BoolValue(e.WaitForCI)
	m.URL = types.StringValue("")
	if e.URL != nil {
		m.URL = types.StringValue(*e.URL)
	}
}

func (r *appEnvironmentResource) put(ctx context.Context, m *appEnvironmentModel, creating bool) (*appEnvironmentAnswer, error) {
	if m.Name.ValueString() == "production" {
		return nil, fmt.Errorf("production comes with the service: change it there")
	}
	body := map[string]any{"branch": nil, "wait_for_ci": m.WaitForCI.ValueBool()}
	if given(m.Branch) {
		body["branch"] = m.Branch.ValueString()
	}
	if creating && given(m.VariablesFrom) {
		body["variables_from"] = m.VariablesFrom.ValueString()
	}
	var e appEnvironmentAnswer
	err := r.c.call(ctx, http.MethodPut, appEnv(m.Service.ValueString(), m.Name.ValueString()), body, &e)
	return &e, err
}

func (r *appEnvironmentResource) Create(ctx context.Context, req resource.CreateRequest, resp *resource.CreateResponse) {
	var m appEnvironmentModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &m)...)
	if resp.Diagnostics.HasError() {
		return
	}
	e, err := r.put(ctx, &m, true)
	if err != nil {
		resp.Diagnostics.AddError("making the environment", err.Error())
		return
	}
	m.fill(e)
	resp.Diagnostics.Append(resp.State.Set(ctx, m)...)
}

func (r *appEnvironmentResource) Read(ctx context.Context, req resource.ReadRequest, resp *resource.ReadResponse) {
	var m appEnvironmentModel
	resp.Diagnostics.Append(req.State.Get(ctx, &m)...)
	if resp.Diagnostics.HasError() {
		return
	}
	var e appEnvironmentAnswer
	if err := r.c.call(ctx, http.MethodGet, appEnv(m.Service.ValueString(), m.Name.ValueString()), nil, &e); notFound(err) {
		resp.State.RemoveResource(ctx)
		return
	} else if err != nil {
		resp.Diagnostics.AddError("reading the environment", err.Error())
		return
	}
	m.fill(&e)
	resp.Diagnostics.Append(resp.State.Set(ctx, m)...)
}

func (r *appEnvironmentResource) Update(ctx context.Context, req resource.UpdateRequest, resp *resource.UpdateResponse) {
	var m appEnvironmentModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &m)...)
	if resp.Diagnostics.HasError() {
		return
	}
	e, err := r.put(ctx, &m, false)
	if err != nil {
		resp.Diagnostics.AddError("changing the environment", err.Error())
		return
	}
	m.fill(e)
	resp.Diagnostics.Append(resp.State.Set(ctx, m)...)
}

func (r *appEnvironmentResource) Delete(ctx context.Context, req resource.DeleteRequest, resp *resource.DeleteResponse) {
	var m appEnvironmentModel
	resp.Diagnostics.Append(req.State.Get(ctx, &m)...)
	if err := r.c.call(ctx, http.MethodDelete, appEnv(m.Service.ValueString(), m.Name.ValueString()), nil, nil); err != nil && !notFound(err) {
		resp.Diagnostics.AddError("deleting the environment", err.Error())
	}
}

// ImportState takes `svc_…/<name>`.
func (r *appEnvironmentResource) ImportState(ctx context.Context, req resource.ImportStateRequest, resp *resource.ImportStateResponse) {
	importParts(ctx, req, resp, "svc_…/<name>", "service", "name")
}

// ── roll_app_link ──

type appLinkResource struct{ c *client }

type appLinkModel struct {
	ID      types.String `tfsdk:"id"`
	Service types.String `tfsdk:"service"`
	Kind    types.String `tfsdk:"kind"`
	Ref     types.String `tfsdk:"ref"`
	Prefix  types.String `tfsdk:"prefix"`
	Vars    types.List   `tfsdk:"vars"`
}

type appLink struct {
	Kind   string   `json:"kind"`
	Ref    string   `json:"ref"`
	Prefix string   `json:"prefix"`
	Vars   []string `json:"vars"`
}

func newAppLinkResource() resource.Resource { return &appLinkResource{} }

func (r *appLinkResource) Metadata(_ context.Context, req resource.MetadataRequest, resp *resource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_app_link"
}

func (r *appLinkResource) Schema(_ context.Context, _ resource.SchemaRequest, resp *resource.SchemaResponse) {
	replace := []planmodifier.String{stringplanmodifier.RequiresReplace()}
	resp.Schema = schema.Schema{
		Description: "A service's link to a roll resource of its project: the resource's connection variables in every environment (a bucket, a stream or a search cluster gets credentials of the link's own, revoked when the link goes), or another service's URLs.",
		Attributes: map[string]schema.Attribute{
			"id":      schema.StringAttribute{Computed: true, PlanModifiers: []planmodifier.String{stringplanmodifier.UseStateForUnknown()}},
			"service": schema.StringAttribute{Required: true, PlanModifiers: replace},
			"kind":    schema.StringAttribute{Required: true, PlanModifiers: replace, Description: "postgres, redis, bucket, stream, nosql, search or service."},
			"ref":     schema.StringAttribute{Required: true, PlanModifiers: replace, Description: "The resource (its id or name; a service's id)."},
			"prefix":  schema.StringAttribute{Optional: true, Computed: true, PlanModifiers: []planmodifier.String{stringplanmodifier.UseStateForUnknown()}, Description: "Before its variables' names (ORDERS → ORDERS_DATABASE_URL). Changing it keeps the credential."},
			"vars":    schema.ListAttribute{Computed: true, ElementType: types.StringType, Description: "The variables it gives, by name (named by the prefix)."},
		},
	}
}

func (r *appLinkResource) Configure(_ context.Context, req resource.ConfigureRequest, _ *resource.ConfigureResponse) {
	r.c = configure(req)
}

func (m *appLinkModel) path() string {
	return appService(m.Service.ValueString()) + "/links/" + url.PathEscape(m.Kind.ValueString()) + "/" + url.PathEscape(m.Ref.ValueString())
}

func (m *appLinkModel) fill(l *appLink) {
	m.ID = types.StringValue(m.Service.ValueString() + "/" + l.Kind + "/" + l.Ref)
	m.Kind, m.Ref, m.Prefix, m.Vars = types.StringValue(l.Kind), types.StringValue(l.Ref), types.StringValue(l.Prefix), strings2list(l.Vars)
}

func (r *appLinkResource) put(ctx context.Context, m *appLinkModel) (*appLink, error) {
	body := map[string]any{}
	if given(m.Prefix) {
		body["prefix"] = m.Prefix.ValueString()
	}
	var l appLink
	err := r.c.call(ctx, http.MethodPut, m.path(), body, &l)
	return &l, err
}

func (r *appLinkResource) Create(ctx context.Context, req resource.CreateRequest, resp *resource.CreateResponse) {
	var m appLinkModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &m)...)
	if resp.Diagnostics.HasError() {
		return
	}
	l, err := r.put(ctx, &m)
	if err != nil {
		resp.Diagnostics.AddError("linking", err.Error())
		return
	}
	m.fill(l)
	resp.Diagnostics.Append(resp.State.Set(ctx, m)...)
}

func (r *appLinkResource) Read(ctx context.Context, req resource.ReadRequest, resp *resource.ReadResponse) {
	var m appLinkModel
	resp.Diagnostics.Append(req.State.Get(ctx, &m)...)
	if resp.Diagnostics.HasError() {
		return
	}
	var all []appLink
	if err := r.c.call(ctx, http.MethodGet, appService(m.Service.ValueString())+"/links", nil, &all); notFound(err) {
		resp.State.RemoveResource(ctx)
		return
	} else if err != nil {
		resp.Diagnostics.AddError("reading the link", err.Error())
		return
	}
	for _, l := range all {
		if l.Kind == m.Kind.ValueString() && l.Ref == m.Ref.ValueString() {
			m.fill(&l)
			resp.Diagnostics.Append(resp.State.Set(ctx, m)...)
			return
		}
	}
	resp.State.RemoveResource(ctx)
}

func (r *appLinkResource) Update(ctx context.Context, req resource.UpdateRequest, resp *resource.UpdateResponse) {
	// Only the prefix changes in place; setting the link again keeps its credential.
	var m appLinkModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &m)...)
	if resp.Diagnostics.HasError() {
		return
	}
	l, err := r.put(ctx, &m)
	if err != nil {
		resp.Diagnostics.AddError("changing the link", err.Error())
		return
	}
	m.fill(l)
	resp.Diagnostics.Append(resp.State.Set(ctx, m)...)
}

func (r *appLinkResource) Delete(ctx context.Context, req resource.DeleteRequest, resp *resource.DeleteResponse) {
	var m appLinkModel
	resp.Diagnostics.Append(req.State.Get(ctx, &m)...)
	if err := r.c.call(ctx, http.MethodDelete, m.path(), nil, nil); err != nil && !notFound(err) {
		resp.Diagnostics.AddError("unlinking", err.Error())
	}
}

// ImportState takes `svc_…/<kind>/<ref>`.
func (r *appLinkResource) ImportState(ctx context.Context, req resource.ImportStateRequest, resp *resource.ImportStateResponse) {
	importParts(ctx, req, resp, "svc_…/<kind>/<ref>", "service", "kind", "ref")
}

// ── roll_app_notification ──

type appNotificationResource struct{ c *client }

type appNotificationModel struct {
	ID           types.String `tfsdk:"id"`
	Project      types.String `tfsdk:"project"`
	Kind         types.String `tfsdk:"kind"`
	URL          types.String `tfsdk:"url"`
	Events       types.List   `tfsdk:"events"`
	Environments types.List   `tfsdk:"environments"`
	Services     types.List   `tfsdk:"services"`
	Previews     types.Bool   `tfsdk:"previews"`
	Host         types.String `tfsdk:"host"`
	Secret       types.String `tfsdk:"secret"`
}

type appNotification struct {
	ID           string   `json:"id"`
	Project      string   `json:"project"`
	Kind         string   `json:"kind"`
	Host         string   `json:"host"`
	Events       []string `json:"events"`
	Environments []string `json:"environments"`
	Services     []string `json:"services"`
	Previews     bool     `json:"previews"`
	Secret       string   `json:"secret"`
}

func newAppNotificationResource() resource.Resource { return &appNotificationResource{} }

func (r *appNotificationResource) Metadata(_ context.Context, req resource.MetadataRequest, resp *resource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_app_notification"
}

func (r *appNotificationResource) Schema(_ context.Context, _ resource.SchemaRequest, resp *resource.SchemaResponse) {
	replace := []planmodifier.String{stringplanmodifier.RequiresReplace()}
	keep := []planmodifier.String{stringplanmodifier.UseStateForUnknown()}
	// The filters keep no state when left out: any change sends them as null, the default.
	resp.Schema = schema.Schema{
		Description: "Where a project's app events go (failed deploys, crash loops, alerts…): a webhook, Slack, Discord, or the project owner's email.",
		Attributes: map[string]schema.Attribute{
			"id":           schema.StringAttribute{Computed: true, PlanModifiers: keep},
			"project":      schema.StringAttribute{Optional: true, Computed: true, PlanModifiers: append(replace, keep...), Description: "prj_…; the provider's project when left out."},
			"kind":         schema.StringAttribute{Optional: true, Computed: true, Default: stringdefault.StaticString("webhook"), PlanModifiers: replace, Description: "webhook, slack, discord or email."},
			"url":          schema.StringAttribute{Optional: true, Sensitive: true, PlanModifiers: replace, Description: "Where it posts (not for email). Never read back: a new URL is a new target."},
			"events":       schema.ListAttribute{Optional: true, Computed: true, ElementType: types.StringType, Description: "Which events (the default set when left out)."},
			"environments": schema.ListAttribute{Optional: true, Computed: true, ElementType: types.StringType, Description: "Only these environments' events (every one's when left out)."},
			"services":     schema.ListAttribute{Optional: true, Computed: true, ElementType: types.StringType, Description: "Only these services' events (ids; every one's when left out)."},
			"previews":     schema.BoolAttribute{Optional: true, Computed: true, Default: booldefault.StaticBool(false), Description: "Preview environments' events too."},
			"host":         schema.StringAttribute{Computed: true, PlanModifiers: keep, Description: "Where it posts, by host."},
			"secret":       schema.StringAttribute{Computed: true, Sensitive: true, PlanModifiers: keep, Description: "A webhook's signing secret (given once, when it's made)."},
		},
	}
}

func (r *appNotificationResource) Configure(_ context.Context, req resource.ConfigureRequest, _ *resource.ConfigureResponse) {
	r.c = configure(req)
}

func (m *appNotificationModel) base() string {
	return "/v1/apps/projects/" + url.PathEscape(m.Project.ValueString()) + "/notifications"
}

func (m *appNotificationModel) fill(n *appNotification) {
	m.ID, m.Project, m.Kind, m.Host, m.Previews = types.StringValue(n.ID), types.StringValue(n.Project), types.StringValue(n.Kind), types.StringValue(n.Host), types.BoolValue(n.Previews)
	m.Events, m.Environments, m.Services = strings2list(n.Events), strings2list(n.Environments), strings2list(n.Services)
	if n.Secret != "" {
		m.Secret = types.StringValue(n.Secret)
	} else if m.Secret.IsUnknown() {
		m.Secret = types.StringValue("")
	}
}

// filters is the body's filters: given lists as they are, the rest null (the default).
func (m *appNotificationModel) filters(ctx context.Context, d *diag.Diagnostics) map[string]any {
	body := map[string]any{"previews": m.Previews.ValueBool()}
	for k, l := range map[string]types.List{"events": m.Events, "environments": m.Environments, "services": m.Services} {
		if s := list2strings(ctx, l, d); s != nil {
			body[k] = s
		} else {
			body[k] = nil
		}
	}
	return body
}

func (r *appNotificationResource) Create(ctx context.Context, req resource.CreateRequest, resp *resource.CreateResponse) {
	var m appNotificationModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &m)...)
	if resp.Diagnostics.HasError() {
		return
	}
	p, ok := project(r.c, m.Project)
	if !ok {
		resp.Diagnostics.AddError("no project", "set project on the resource or the provider")
		return
	}
	m.Project = types.StringValue(p)
	body := m.filters(ctx, &resp.Diagnostics)
	for k, v := range body {
		if v == nil {
			delete(body, k)
		}
	}
	body["kind"] = m.Kind.ValueString()
	if given(m.URL) {
		body["url"] = m.URL.ValueString()
	}
	var n appNotification
	if err := r.c.call(ctx, http.MethodPost, m.base(), body, &n); err != nil {
		resp.Diagnostics.AddError("adding the notification target", err.Error())
		return
	}
	m.fill(&n)
	resp.Diagnostics.Append(resp.State.Set(ctx, m)...)
}

func (r *appNotificationResource) Read(ctx context.Context, req resource.ReadRequest, resp *resource.ReadResponse) {
	var m appNotificationModel
	resp.Diagnostics.Append(req.State.Get(ctx, &m)...)
	if resp.Diagnostics.HasError() {
		return
	}
	var all []appNotification
	if err := r.c.call(ctx, http.MethodGet, m.base(), nil, &all); notFound(err) {
		resp.State.RemoveResource(ctx)
		return
	} else if err != nil {
		resp.Diagnostics.AddError("reading the notification target", err.Error())
		return
	}
	for _, n := range all {
		if n.ID == m.ID.ValueString() {
			m.fill(&n)
			resp.Diagnostics.Append(resp.State.Set(ctx, m)...)
			return
		}
	}
	resp.State.RemoveResource(ctx)
}

func (r *appNotificationResource) Update(ctx context.Context, req resource.UpdateRequest, resp *resource.UpdateResponse) {
	var m, state appNotificationModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &m)...)
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	if resp.Diagnostics.HasError() {
		return
	}
	var n appNotification
	if err := r.c.call(ctx, http.MethodPatch, m.base()+"/"+url.PathEscape(m.ID.ValueString()), m.filters(ctx, &resp.Diagnostics), &n); err != nil {
		resp.Diagnostics.AddError("changing the notification target", err.Error())
		return
	}
	m.Secret = state.Secret
	m.fill(&n)
	resp.Diagnostics.Append(resp.State.Set(ctx, m)...)
}

func (r *appNotificationResource) Delete(ctx context.Context, req resource.DeleteRequest, resp *resource.DeleteResponse) {
	var m appNotificationModel
	resp.Diagnostics.Append(req.State.Get(ctx, &m)...)
	if err := r.c.call(ctx, http.MethodDelete, m.base()+"/"+url.PathEscape(m.ID.ValueString()), nil, nil); err != nil && !notFound(err) {
		resp.Diagnostics.AddError("removing the notification target", err.Error())
	}
}

// ImportState takes `prj_…/<id>` (its URL and a webhook's secret aren't read back).
func (r *appNotificationResource) ImportState(ctx context.Context, req resource.ImportStateRequest, resp *resource.ImportStateResponse) {
	importParts(ctx, req, resp, "prj_…/<id>", "project", "id")
}

// ── data "roll_app_service" ──

type appServiceDataSource struct{ c *client }

type appServiceDataModel struct {
	ID           types.String `tfsdk:"id"`
	Project      types.String `tfsdk:"project"`
	Name         types.String `tfsdk:"name"`
	SourceKind   types.String `tfsdk:"source_kind"`
	Type         types.String `tfsdk:"type"`
	URL          types.String `tfsdk:"url"`
	Environments types.Map    `tfsdk:"environments"`
}

func newAppServiceDataSource() datasource.DataSource { return &appServiceDataSource{} }

func (d *appServiceDataSource) Metadata(_ context.Context, req datasource.MetadataRequest, resp *datasource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_app_service"
}

func (d *appServiceDataSource) Schema(_ context.Context, _ datasource.SchemaRequest, resp *datasource.SchemaResponse) {
	resp.Schema = dschema.Schema{
		Description: "A roll apps service made elsewhere (the dashboard, roll.toml, another configuration), by id, or by name in a project.",
		Attributes: map[string]dschema.Attribute{
			"id":           dschema.StringAttribute{Optional: true, Computed: true, Description: "svc_…; or give name."},
			"project":      dschema.StringAttribute{Optional: true, Computed: true, Description: "With name: prj_… (the provider's project when left out)."},
			"name":         dschema.StringAttribute{Optional: true, Computed: true},
			"source_kind":  dschema.StringAttribute{Computed: true, Description: "repo, git, image or upload."},
			"type":         dschema.StringAttribute{Computed: true, Description: "web, worker or cron."},
			"url":          dschema.StringAttribute{Computed: true, Description: "Production's URL."},
			"environments": dschema.MapAttribute{Computed: true, ElementType: types.StringType, Description: "Each environment's URL, by name."},
		},
	}
}

func (d *appServiceDataSource) Configure(_ context.Context, req datasource.ConfigureRequest, _ *datasource.ConfigureResponse) {
	d.c, _ = req.ProviderData.(*client)
}

func (d *appServiceDataSource) Read(ctx context.Context, req datasource.ReadRequest, resp *datasource.ReadResponse) {
	var m appServiceDataModel
	resp.Diagnostics.Append(req.Config.Get(ctx, &m)...)
	if resp.Diagnostics.HasError() {
		return
	}
	id := m.ID.ValueString()
	if !given(m.ID) {
		p, ok := project(d.c, m.Project)
		if !given(m.Name) || !ok {
			resp.Diagnostics.AddError("which service", "give id, or name (and project, unless the provider has one)")
			return
		}
		var all []appServiceAnswer
		if err := d.c.call(ctx, http.MethodGet, "/v1/apps/projects/"+url.PathEscape(p)+"/services", nil, &all); err != nil {
			resp.Diagnostics.AddError("listing the project's services", err.Error())
			return
		}
		for _, s := range all {
			if s.Name == m.Name.ValueString() {
				id = s.ID
			}
		}
		if id == "" {
			resp.Diagnostics.AddError("no such service", fmt.Sprintf("%s has no service %q", p, m.Name.ValueString()))
			return
		}
	}
	var s appServiceAnswer
	if err := d.c.call(ctx, http.MethodGet, appService(id), nil, &s); err != nil {
		resp.Diagnostics.AddError("reading the service", err.Error())
		return
	}
	envs := map[string]attr.Value{}
	m.URL = types.StringValue("")
	for _, e := range s.Environments {
		envs[e.Name] = types.StringValue(e.URL)
		if e.Name == "production" {
			m.URL = types.StringValue(e.URL)
		}
	}
	m.ID, m.Project, m.Name, m.SourceKind, m.Type = types.StringValue(s.ID), types.StringValue(s.Project), types.StringValue(s.Name), types.StringValue(s.Source.Kind), types.StringValue(s.Run.Type)
	m.Environments = types.MapValueMust(types.StringType, envs)
	resp.Diagnostics.Append(resp.State.Set(ctx, m)...)
}
