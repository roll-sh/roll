package provider

import (
	"context"
	"fmt"
	"net/http"
	"net/url"
	"strings"

	"github.com/hashicorp/terraform-plugin-framework/diag"
	"github.com/hashicorp/terraform-plugin-framework/path"
	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/booldefault"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/boolplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/int64planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringdefault"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/types"
)

// roll apps (docs/apps/http-api.md): a service, its variables and its custom domains as code.

func appService(id string) string { return "/v1/apps/services/" + url.PathEscape(id) }

func appEnv(id, env string) string { return appService(id) + "/environments/" + url.PathEscape(env) }

// ── roll_app_service ──

type appServiceResource struct{ c *client }

type appServiceModel struct {
	ID           types.String `tfsdk:"id"`
	Project      types.String `tfsdk:"project"`
	Name         types.String `tfsdk:"name"`
	Image        types.String `tfsdk:"image"`
	Repo         types.String `tfsdk:"repo"`
	GitURL       types.String `tfsdk:"git_url"`
	Root         types.String `tfsdk:"root"`
	Branch       types.String `tfsdk:"branch"`
	Type         types.String `tfsdk:"type"`
	Port         types.Int64  `tfsdk:"port"`
	Start        types.String `tfsdk:"start"`
	MinInstances types.Int64  `tfsdk:"min_instances"`
	MaxInstances types.Int64  `tfsdk:"max_instances"`
	MemoryMB     types.Int64  `tfsdk:"memory_mb"`
	URL          types.String `tfsdk:"url"`
	GitUsername  types.String `tfsdk:"git_username"`
	GitToken     types.String `tfsdk:"git_token"`
}

// The API's service, as much of it as the resource keeps.
type appServiceAnswer struct {
	ID      string `json:"id"`
	Project string `json:"project"`
	Name    string `json:"name"`
	Source  struct {
		Kind  string `json:"kind"`
		Image string `json:"image"`
		Repo  string `json:"repo"`
		URL   string `json:"url"`
		Root  string `json:"root"`
	} `json:"source"`
	Run struct {
		Type         string `json:"type"`
		Port         *int64 `json:"port"`
		Start        string `json:"start"`
		MinInstances int64  `json:"min_instances"`
		MaxInstances int64  `json:"max_instances"`
		MemoryMB     int64  `json:"memory_mb"`
	} `json:"run"`
	Environments []struct {
		Name string `json:"name"`
		URL  string `json:"url"`
	} `json:"environments"`
	GitLogin *struct {
		Username string `json:"username"`
	} `json:"git_login"`
}

func newAppServiceResource() resource.Resource { return &appServiceResource{} }

func (r *appServiceResource) Metadata(_ context.Context, req resource.MetadataRequest, resp *resource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_app_service"
}

func (r *appServiceResource) Schema(_ context.Context, _ resource.SchemaRequest, resp *resource.SchemaResponse) {
	keep := []planmodifier.String{stringplanmodifier.UseStateForUnknown()}
	keepInt := []planmodifier.Int64{int64planmodifier.UseStateForUnknown()}
	replace := []planmodifier.String{stringplanmodifier.RequiresReplace()}
	resp.Schema = schema.Schema{
		Description: "A roll apps service: your code, or an image, next to your roll databases. A repo, Git URL or image service deploys production when it's made; one with none of them deploys what `roll up` uploads.",
		Attributes: map[string]schema.Attribute{
			"id":            schema.StringAttribute{Computed: true, PlanModifiers: keep},
			"project":       schema.StringAttribute{Optional: true, Computed: true, PlanModifiers: append(replace, keep...), Description: "prj_…; the provider's project when left out."},
			"name":          schema.StringAttribute{Required: true, Description: "1–32 of a-z, 0-9 and -: part of its URL."},
			"image":         schema.StringAttribute{Optional: true, Description: "A public image (its digest is pinned at each deploy)."},
			"repo":          schema.StringAttribute{Optional: true, Description: "A roll code repo (owner/name)."},
			"git_url":       schema.StringAttribute{Optional: true, Description: "A public Git repository's https:// URL."},
			"root":          schema.StringAttribute{Optional: true, Description: "With repo or git_url: the folder that builds."},
			"branch":        schema.StringAttribute{Optional: true, PlanModifiers: replace, Description: "With repo or git_url: the branch production follows (its default when left out)."},
			"type":          schema.StringAttribute{Optional: true, Computed: true, Default: stringdefault.StaticString("web"), Description: "web, worker or cron."},
			"port":          schema.Int64Attribute{Optional: true, Computed: true, PlanModifiers: keepInt, Description: "The port it listens on."},
			"start":         schema.StringAttribute{Optional: true, Description: "Its start command, over the detected one."},
			"min_instances": schema.Int64Attribute{Optional: true, Computed: true, PlanModifiers: keepInt, Description: "0 sleeps when idle."},
			"max_instances": schema.Int64Attribute{Optional: true, Computed: true, PlanModifiers: keepInt},
			"memory_mb":     schema.Int64Attribute{Optional: true, Computed: true, PlanModifiers: keepInt},
			"url":           schema.StringAttribute{Computed: true, PlanModifiers: keep, Description: "Production's URL."},
			"git_username":  schema.StringAttribute{Optional: true, Description: "With git_url: a private repository's login (a deploy token's user, say)."},
			"git_token":     schema.StringAttribute{Optional: true, Sensitive: true, Description: "With git_username: its token or app password. Sealed by roll, sent only to the URL's host, never read back."},
		},
	}
}

func (r *appServiceResource) Configure(_ context.Context, req resource.ConfigureRequest, _ *resource.ConfigureResponse) {
	if c, ok := req.ProviderData.(*client); ok {
		r.c = c
	}
}

func given(v types.String) bool { return !v.IsNull() && !v.IsUnknown() && v.ValueString() != "" }

func givenInt(v types.Int64) bool { return !v.IsNull() && !v.IsUnknown() }

func (m *appServiceModel) source() map[string]any {
	with := func(s map[string]any) map[string]any {
		if given(m.Root) {
			s["root"] = m.Root.ValueString()
		}
		return s
	}
	switch {
	case given(m.Image):
		return map[string]any{"kind": "image", "image": m.Image.ValueString()}
	case given(m.Repo):
		return with(map[string]any{"kind": "repo", "repo": m.Repo.ValueString()})
	case given(m.GitURL):
		return with(map[string]any{"kind": "git", "url": m.GitURL.ValueString()})
	}
	return map[string]any{"kind": "upload"}
}

// checkLogin: a login is both halves, on a Git URL.
func (m *appServiceModel) checkLogin(d *diag.Diagnostics) {
	if given(m.GitUsername) != given(m.GitToken) {
		d.AddAttributeError(path.Root("git_token"), "half a login", "git_username and git_token go together")
	}
	if given(m.GitUsername) && !given(m.GitURL) {
		d.AddAttributeError(path.Root("git_username"), "a login without a Git URL", "a login is for a git_url source")
	}
}

// gitLogin is the private repository's login, when both halves are given.
func (m *appServiceModel) gitLogin() map[string]any {
	if !given(m.GitURL) || !given(m.GitUsername) || !given(m.GitToken) {
		return nil
	}
	return map[string]any{"username": m.GitUsername.ValueString(), "password": m.GitToken.ValueString()}
}

func (m *appServiceModel) run() map[string]any {
	run := map[string]any{"type": m.Type.ValueString()}
	if givenInt(m.Port) {
		run["port"] = m.Port.ValueInt64()
	}
	if given(m.Start) {
		run["start"] = m.Start.ValueString()
	}
	for k, v := range map[string]types.Int64{"min_instances": m.MinInstances, "max_instances": m.MaxInstances, "memory_mb": m.MemoryMB} {
		if givenInt(v) {
			run[k] = v.ValueInt64()
		}
	}
	return run
}

func optional(v string) types.String {
	if v == "" {
		return types.StringNull()
	}
	return types.StringValue(v)
}

// fill sets the model from the API's service.
func (m *appServiceModel) fill(s *appServiceAnswer) {
	m.ID, m.Project, m.Name = types.StringValue(s.ID), types.StringValue(s.Project), types.StringValue(s.Name)
	m.Type = types.StringValue(s.Run.Type)
	if s.Run.Port != nil {
		m.Port = types.Int64Value(*s.Run.Port)
	} else {
		m.Port = types.Int64Null()
	}
	m.MinInstances, m.MaxInstances, m.MemoryMB = types.Int64Value(s.Run.MinInstances), types.Int64Value(s.Run.MaxInstances), types.Int64Value(s.Run.MemoryMB)
	m.Image, m.Repo, m.GitURL, m.Root = optional(s.Source.Image), optional(s.Source.Repo), optional(s.Source.URL), optional(s.Source.Root)
	m.Start = optional(s.Run.Start)
	// A login removed elsewhere shows as a change (its token is never read back: the state's stays).
	if s.GitLogin != nil {
		m.GitUsername = types.StringValue(s.GitLogin.Username)
	} else {
		m.GitUsername = types.StringNull()
	}
	m.URL = types.StringValue("")
	for _, e := range s.Environments {
		if e.Name == "production" {
			m.URL = types.StringValue(e.URL)
		}
	}
}

func (r *appServiceResource) Create(ctx context.Context, req resource.CreateRequest, resp *resource.CreateResponse) {
	var m appServiceModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &m)...)
	p, ok := project(r.c, m.Project)
	if !ok {
		resp.Diagnostics.AddAttributeError(path.Root("project"), "which project?", "set project here or on the provider (ROLL_PROJECT)")
	}
	m.checkLogin(&resp.Diagnostics)
	if resp.Diagnostics.HasError() {
		return
	}
	body := map[string]any{"project": p, "name": m.Name.ValueString(), "source": m.source(), "run": m.run()}
	if login := m.gitLogin(); login != nil {
		body["git_login"] = login
	}
	if given(m.Branch) {
		body["branch"] = m.Branch.ValueString()
	}
	var s appServiceAnswer
	if err := r.c.call(ctx, http.MethodPost, "/v1/apps/services", body, &s); err != nil {
		resp.Diagnostics.AddError("making the service", err.Error())
		return
	}
	m.fill(&s)
	resp.Diagnostics.Append(resp.State.Set(ctx, m)...)
}

func (r *appServiceResource) Read(ctx context.Context, req resource.ReadRequest, resp *resource.ReadResponse) {
	var m appServiceModel
	resp.Diagnostics.Append(req.State.Get(ctx, &m)...)
	if resp.Diagnostics.HasError() {
		return
	}
	var s appServiceAnswer
	if err := r.c.call(ctx, http.MethodGet, appService(m.ID.ValueString()), nil, &s); notFound(err) {
		resp.State.RemoveResource(ctx)
		return
	} else if err != nil {
		resp.Diagnostics.AddError("reading the service", err.Error())
		return
	}
	m.fill(&s)
	if m.Type.ValueString() == "" {
		m.Type = types.StringValue("web")
	}
	resp.Diagnostics.Append(resp.State.Set(ctx, m)...)
}

func (r *appServiceResource) Update(ctx context.Context, req resource.UpdateRequest, resp *resource.UpdateResponse) {
	var m, prior appServiceModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &m)...)
	resp.Diagnostics.Append(req.State.Get(ctx, &prior)...)
	m.checkLogin(&resp.Diagnostics)
	if resp.Diagnostics.HasError() {
		return
	}
	var s appServiceAnswer
	body := map[string]any{"name": m.Name.ValueString(), "source": m.source(), "run": m.run()}
	if err := r.c.call(ctx, http.MethodPatch, appService(prior.ID.ValueString()), body, &s); err != nil {
		resp.Diagnostics.AddError("updating the service", err.Error())
		return
	}
	// The login: set again when it or the URL changed (another host drops it), removed when it's gone.
	login, path := m.gitLogin(), appService(prior.ID.ValueString())+"/git-login"
	changed := !m.GitUsername.Equal(prior.GitUsername) || !m.GitToken.Equal(prior.GitToken) || !m.GitURL.Equal(prior.GitURL)
	if login != nil && changed {
		if err := r.c.call(ctx, http.MethodPut, path, login, &s); err != nil {
			// The rest is changed: the state says so, with the login as it was, so the next
			// plan sets it again.
			m.fill(&s)
			m.GitToken = prior.GitToken
			resp.Diagnostics.Append(resp.State.Set(ctx, m)...)
			resp.Diagnostics.AddError("setting the Git login", err.Error())
			return
		}
	} else if login == nil && given(prior.GitUsername) && given(m.GitURL) {
		if err := r.c.call(ctx, http.MethodDelete, path, nil, &s); err != nil && !notFound(err) {
			resp.Diagnostics.AddError("removing the Git login", err.Error())
			return
		}
	}
	m.fill(&s)
	resp.Diagnostics.Append(resp.State.Set(ctx, m)...)
}

func (r *appServiceResource) Delete(ctx context.Context, req resource.DeleteRequest, resp *resource.DeleteResponse) {
	var m appServiceModel
	resp.Diagnostics.Append(req.State.Get(ctx, &m)...)
	if err := r.c.call(ctx, http.MethodDelete, appService(m.ID.ValueString()), nil, nil); err != nil && !notFound(err) {
		resp.Diagnostics.AddError("deleting the service", err.Error())
	}
}

func (r *appServiceResource) ImportState(ctx context.Context, req resource.ImportStateRequest, resp *resource.ImportStateResponse) {
	resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root("id"), req.ID)...)
}

// ── roll_app_variable ──

type appVariableResource struct{ c *client }

type appVariableModel struct {
	ID          types.String `tfsdk:"id"`
	Service     types.String `tfsdk:"service"`
	Environment types.String `tfsdk:"environment"`
	Name        types.String `tfsdk:"name"`
	Value       types.String `tfsdk:"value"`
	Sealed      types.Bool   `tfsdk:"sealed"`
}

func newAppVariableResource() resource.Resource { return &appVariableResource{} }

func (r *appVariableResource) Metadata(_ context.Context, req resource.MetadataRequest, resp *resource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_app_variable"
}

func (r *appVariableResource) Schema(_ context.Context, _ resource.SchemaRequest, resp *resource.SchemaResponse) {
	replace := []planmodifier.String{stringplanmodifier.RequiresReplace()}
	resp.Schema = schema.Schema{
		Description: "A variable of a service's environment. A project's secrets belong in roll secrets (roll_secret): those reach the service by themselves.",
		Attributes: map[string]schema.Attribute{
			"id":          schema.StringAttribute{Computed: true, PlanModifiers: []planmodifier.String{stringplanmodifier.UseStateForUnknown()}},
			"service":     schema.StringAttribute{Required: true, PlanModifiers: replace, Description: "svc_…"},
			"environment": schema.StringAttribute{Optional: true, Computed: true, Default: stringdefault.StaticString("production"), PlanModifiers: replace},
			"name":        schema.StringAttribute{Required: true, PlanModifiers: replace},
			"value":       schema.StringAttribute{Required: true, Sensitive: true, Description: "Its value; `${{ VAR }}` and `${{ api.VAR }}` reference others."},
			"sealed":      schema.BoolAttribute{Optional: true, Computed: true, Default: booldefault.StaticBool(false), PlanModifiers: []planmodifier.Bool{boolplanmodifier.RequiresReplace()}, Description: "Never shown again (a sealed variable can't be unsealed: changing this makes it again)."},
		},
	}
}

func (r *appVariableResource) Configure(_ context.Context, req resource.ConfigureRequest, _ *resource.ConfigureResponse) {
	if c, ok := req.ProviderData.(*client); ok {
		r.c = c
	}
}

func (r *appVariableResource) write(ctx context.Context, m *appVariableModel) error {
	change := map[string]any{"set": map[string]string{m.Name.ValueString(): m.Value.ValueString()}}
	if m.Sealed.ValueBool() {
		change["seal"] = []string{m.Name.ValueString()}
	}
	m.ID = types.StringValue(m.Service.ValueString() + "/" + m.Environment.ValueString() + "/" + m.Name.ValueString())
	return r.c.call(ctx, http.MethodPatch, appEnv(m.Service.ValueString(), m.Environment.ValueString())+"/variables", change, nil)
}

func (r *appVariableResource) Create(ctx context.Context, req resource.CreateRequest, resp *resource.CreateResponse) {
	var m appVariableModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &m)...)
	if resp.Diagnostics.HasError() {
		return
	}
	if err := r.write(ctx, &m); err != nil {
		resp.Diagnostics.AddError("setting the variable", err.Error())
		return
	}
	resp.Diagnostics.Append(resp.State.Set(ctx, m)...)
}

// Read checks it's still there (and sealed as said); a value isn't read back: a sealed one never
// shows again, and an open one would need reveal.
func (r *appVariableResource) Read(ctx context.Context, req resource.ReadRequest, resp *resource.ReadResponse) {
	var m appVariableModel
	resp.Diagnostics.Append(req.State.Get(ctx, &m)...)
	if resp.Diagnostics.HasError() {
		return
	}
	var v struct {
		Vars []struct {
			Name   string `json:"name"`
			Source string `json:"source"`
			Sealed bool   `json:"sealed"`
		} `json:"vars"`
	}
	if err := r.c.call(ctx, http.MethodGet, appEnv(m.Service.ValueString(), m.Environment.ValueString())+"/variables", nil, &v); notFound(err) {
		resp.State.RemoveResource(ctx)
		return
	} else if err != nil {
		resp.Diagnostics.AddError("reading the variable", err.Error())
		return
	}
	for _, x := range v.Vars {
		if x.Name == m.Name.ValueString() && (x.Source == "service" || x.Source == "reference") {
			m.Sealed = types.BoolValue(x.Sealed)
			resp.Diagnostics.Append(resp.State.Set(ctx, m)...)
			return
		}
	}
	resp.State.RemoveResource(ctx)
}

func (r *appVariableResource) Update(ctx context.Context, req resource.UpdateRequest, resp *resource.UpdateResponse) {
	var m appVariableModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &m)...)
	if resp.Diagnostics.HasError() {
		return
	}
	if err := r.write(ctx, &m); err != nil {
		resp.Diagnostics.AddError("setting the variable", err.Error())
		return
	}
	resp.Diagnostics.Append(resp.State.Set(ctx, m)...)
}

func (r *appVariableResource) Delete(ctx context.Context, req resource.DeleteRequest, resp *resource.DeleteResponse) {
	var m appVariableModel
	resp.Diagnostics.Append(req.State.Get(ctx, &m)...)
	err := r.c.call(ctx, http.MethodPatch, appEnv(m.Service.ValueString(), m.Environment.ValueString())+"/variables", map[string]any{"unset": []string{m.Name.ValueString()}}, nil)
	if err != nil && !notFound(err) {
		resp.Diagnostics.AddError("unsetting the variable", err.Error())
	}
}

// ── roll_app_domain ──

type appDomainResource struct{ c *client }

type appDomainModel struct {
	ID          types.String `tfsdk:"id"`
	Service     types.String `tfsdk:"service"`
	Environment types.String `tfsdk:"environment"`
	Hostname    types.String `tfsdk:"hostname"`
	Status      types.String `tfsdk:"status"`
	Target      types.String `tfsdk:"target"`
}

type appDomain struct {
	Hostname string `json:"hostname"`
	Status   string `json:"status"`
	Target   string `json:"target"`
}

func newAppDomainResource() resource.Resource { return &appDomainResource{} }

func (r *appDomainResource) Metadata(_ context.Context, req resource.MetadataRequest, resp *resource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_app_domain"
}

func (r *appDomainResource) Schema(_ context.Context, _ resource.SchemaRequest, resp *resource.SchemaResponse) {
	replace := []planmodifier.String{stringplanmodifier.RequiresReplace()}
	resp.Schema = schema.Schema{
		Description: "A custom domain of a service's environment. Point its DNS at `target` (a CNAME, or the TXT challenge for an apex): it goes active once it resolves.",
		Attributes: map[string]schema.Attribute{
			"id":          schema.StringAttribute{Computed: true, PlanModifiers: []planmodifier.String{stringplanmodifier.UseStateForUnknown()}},
			"service":     schema.StringAttribute{Required: true, PlanModifiers: replace},
			"environment": schema.StringAttribute{Optional: true, Computed: true, Default: stringdefault.StaticString("production"), PlanModifiers: replace},
			"hostname":    schema.StringAttribute{Required: true, PlanModifiers: replace},
			"status":      schema.StringAttribute{Computed: true},
			"target":      schema.StringAttribute{Computed: true, PlanModifiers: []planmodifier.String{stringplanmodifier.UseStateForUnknown()}, Description: "What its CNAME points at."},
		},
	}
}

func (r *appDomainResource) Configure(_ context.Context, req resource.ConfigureRequest, _ *resource.ConfigureResponse) {
	if c, ok := req.ProviderData.(*client); ok {
		r.c = c
	}
}

func (m *appDomainModel) fill(d *appDomain) {
	m.ID = types.StringValue(m.Service.ValueString() + "/" + m.Environment.ValueString() + "/" + d.Hostname)
	m.Hostname, m.Status, m.Target = types.StringValue(d.Hostname), types.StringValue(d.Status), types.StringValue(d.Target)
}

func (r *appDomainResource) Create(ctx context.Context, req resource.CreateRequest, resp *resource.CreateResponse) {
	var m appDomainModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &m)...)
	if resp.Diagnostics.HasError() {
		return
	}
	var d appDomain
	if err := r.c.call(ctx, http.MethodPost, appEnv(m.Service.ValueString(), m.Environment.ValueString())+"/domains", map[string]any{"hostname": m.Hostname.ValueString()}, &d); err != nil {
		resp.Diagnostics.AddError("adding the domain", err.Error())
		return
	}
	m.fill(&d)
	resp.Diagnostics.Append(resp.State.Set(ctx, m)...)
}

func (r *appDomainResource) Read(ctx context.Context, req resource.ReadRequest, resp *resource.ReadResponse) {
	var m appDomainModel
	resp.Diagnostics.Append(req.State.Get(ctx, &m)...)
	if resp.Diagnostics.HasError() {
		return
	}
	var all []appDomain
	if err := r.c.call(ctx, http.MethodGet, appEnv(m.Service.ValueString(), m.Environment.ValueString())+"/domains", nil, &all); notFound(err) {
		resp.State.RemoveResource(ctx)
		return
	} else if err != nil {
		resp.Diagnostics.AddError("reading the domain", err.Error())
		return
	}
	for _, d := range all {
		if strings.EqualFold(d.Hostname, m.Hostname.ValueString()) {
			m.fill(&d)
			resp.Diagnostics.Append(resp.State.Set(ctx, m)...)
			return
		}
	}
	resp.State.RemoveResource(ctx)
}

func (r *appDomainResource) Update(ctx context.Context, req resource.UpdateRequest, resp *resource.UpdateResponse) {
	// Everything it has makes it again (RequiresReplace): nothing changes in place.
	var m appDomainModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &m)...)
	resp.Diagnostics.Append(resp.State.Set(ctx, m)...)
}

func (r *appDomainResource) Delete(ctx context.Context, req resource.DeleteRequest, resp *resource.DeleteResponse) {
	var m appDomainModel
	resp.Diagnostics.Append(req.State.Get(ctx, &m)...)
	err := r.c.call(ctx, http.MethodDelete, appEnv(m.Service.ValueString(), m.Environment.ValueString())+"/domains/"+url.PathEscape(m.Hostname.ValueString()), nil, nil)
	if err != nil && !notFound(err) {
		resp.Diagnostics.AddError("removing the domain", err.Error())
	}
}

// ImportState takes `svc_…/<environment>/<hostname>`.
func (r *appDomainResource) ImportState(ctx context.Context, req resource.ImportStateRequest, resp *resource.ImportStateResponse) {
	parts := strings.SplitN(req.ID, "/", 3)
	if len(parts) != 3 {
		resp.Diagnostics.AddError("import id", fmt.Sprintf("%q isn't svc_…/<environment>/<hostname>", req.ID))
		return
	}
	for i, k := range []string{"service", "environment", "hostname"} {
		resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root(k), parts[i])...)
	}
}
