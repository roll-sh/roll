package provider

import (
	"context"
	"net/http"
	"net/url"

	"github.com/hashicorp/terraform-plugin-framework/diag"
	"github.com/hashicorp/terraform-plugin-framework/path"
	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/boolplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/int64planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/listplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/types"
)

// What a project registers with roll secrets itself (docs/secrets.md, "Links"): a CA, an SSH CA,
// an encryption key, a database outside roll. roll changes none in place, so every attribute
// replaces it; reads find it in the project's list.
type registration[M any] struct {
	c *client
	// typeName after the provider's (`_secrets_ca`), route under the project (`cas`), what it is.
	typeName, route, what string
	schema                map[string]schema.Attribute
	description           string
	// keys gives the model's project and id.
	keys func(*M) (*types.String, *types.String)
	// body is what's posted; cfg reads a write-only attribute from the configuration.
	body func(ctx context.Context, m *M, cfg func(string) types.String, d *diag.Diagnostics) map[string]any
	// fill copies an answer (made, or one of the list's) into the model.
	fill func(m *M, v map[string]any)
	// deletePath is the DELETE path under the project, when it isn't route/id.
	deletePath func(m *M) string
}

func (r *registration[M]) Metadata(_ context.Context, req resource.MetadataRequest, resp *resource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + r.typeName
}

func (r *registration[M]) Schema(_ context.Context, _ resource.SchemaRequest, resp *resource.SchemaResponse) {
	attrs := map[string]schema.Attribute{
		"id":      schema.StringAttribute{Computed: true, PlanModifiers: []planmodifier.String{stringplanmodifier.UseStateForUnknown()}},
		"project": schema.StringAttribute{Optional: true, Computed: true, PlanModifiers: []planmodifier.String{stringplanmodifier.RequiresReplace(), stringplanmodifier.UseStateForUnknown()}, Description: "prj_…; the provider's project when left out."},
	}
	for k, v := range r.schema {
		attrs[k] = v
	}
	resp.Schema = schema.Schema{Description: r.description, Attributes: attrs}
}

func (r *registration[M]) Configure(_ context.Context, req resource.ConfigureRequest, _ *resource.ConfigureResponse) {
	if c, ok := req.ProviderData.(*client); ok {
		r.c = c
	}
}

func (r *registration[M]) base(project string) string {
	return "/v1/secrets/projects/" + url.PathEscape(project) + "/" + r.route
}

func (r *registration[M]) Create(ctx context.Context, req resource.CreateRequest, resp *resource.CreateResponse) {
	var m M
	resp.Diagnostics.Append(req.Plan.Get(ctx, &m)...)
	proj, _ := r.keys(&m)
	p, ok := project(r.c, *proj)
	if !ok {
		resp.Diagnostics.AddError("which project?", "set project here or on the provider (ROLL_PROJECT)")
	}
	if resp.Diagnostics.HasError() {
		return
	}
	*proj = types.StringValue(p)
	cfg := func(name string) types.String {
		var v types.String
		resp.Diagnostics.Append(req.Config.GetAttribute(ctx, path.Root(name), &v)...)
		return v
	}
	body := r.body(ctx, &m, cfg, &resp.Diagnostics)
	if resp.Diagnostics.HasError() {
		return
	}
	var made map[string]any
	if err := r.c.call(ctx, http.MethodPost, r.base(p), body, &made); err != nil {
		resp.Diagnostics.AddError("creating the "+r.what, err.Error())
		return
	}
	r.fill(&m, made)
	resp.Diagnostics.Append(resp.State.Set(ctx, m)...)
}

func (r *registration[M]) Read(ctx context.Context, req resource.ReadRequest, resp *resource.ReadResponse) {
	var m M
	resp.Diagnostics.Append(req.State.Get(ctx, &m)...)
	if resp.Diagnostics.HasError() {
		return
	}
	proj, id := r.keys(&m)
	var all []map[string]any
	if err := r.c.call(ctx, http.MethodGet, r.base(proj.ValueString()), nil, &all); err != nil {
		resp.Diagnostics.AddError("reading the "+r.what, err.Error())
		return
	}
	for _, v := range all {
		if s, _ := v["id"].(string); s == id.ValueString() {
			r.fill(&m, v)
			resp.Diagnostics.Append(resp.State.Set(ctx, m)...)
			return
		}
	}
	resp.State.RemoveResource(ctx)
}

func (r *registration[M]) Update(_ context.Context, _ resource.UpdateRequest, resp *resource.UpdateResponse) {
	resp.Diagnostics.AddError(r.what+"s don't change in place", "every attribute replaces it")
}

func (r *registration[M]) Delete(ctx context.Context, req resource.DeleteRequest, resp *resource.DeleteResponse) {
	var m M
	resp.Diagnostics.Append(req.State.Get(ctx, &m)...)
	if resp.Diagnostics.HasError() {
		return
	}
	proj, id := r.keys(&m)
	p := r.base(proj.ValueString()) + "/" + url.PathEscape(id.ValueString())
	if r.deletePath != nil {
		p = "/v1/secrets/projects/" + url.PathEscape(proj.ValueString()) + "/" + r.deletePath(&m)
	}
	if err := r.c.call(ctx, http.MethodDelete, p, nil, nil); err != nil && !notFound(err) {
		resp.Diagnostics.AddError("deleting the "+r.what, err.Error())
	}
}

// Attributes that replace the registration when they change.
func reqString(desc string) schema.StringAttribute {
	return schema.StringAttribute{Required: true, Description: desc, PlanModifiers: []planmodifier.String{stringplanmodifier.RequiresReplace()}}
}

func optString(desc string) schema.StringAttribute {
	return schema.StringAttribute{Optional: true, Description: desc, PlanModifiers: []planmodifier.String{stringplanmodifier.RequiresReplace()}}
}

func outString(desc string) schema.StringAttribute {
	return schema.StringAttribute{Computed: true, Description: desc, PlanModifiers: []planmodifier.String{stringplanmodifier.UseStateForUnknown()}}
}

func strList(required bool, desc string) schema.ListAttribute {
	return schema.ListAttribute{Required: required, Optional: !required, ElementType: types.StringType, Description: desc, PlanModifiers: []planmodifier.List{listplanmodifier.RequiresReplace()}}
}

func optInt(desc string) schema.Int64Attribute {
	return schema.Int64Attribute{Optional: true, Computed: true, Description: desc, PlanModifiers: []planmodifier.Int64{int64planmodifier.RequiresReplace(), int64planmodifier.UseStateForUnknown()}}
}

func optBool(desc string) schema.BoolAttribute {
	return schema.BoolAttribute{Optional: true, Computed: true, Description: desc, PlanModifiers: []planmodifier.Bool{boolplanmodifier.RequiresReplace(), boolplanmodifier.UseStateForUnknown()}}
}

// JSON answers' fields as the model's types.
func jStr(v map[string]any, k string) types.String {
	if s, ok := v[k].(string); ok {
		return types.StringValue(s)
	}
	return types.StringNull()
}

func jInt(v map[string]any, k string) types.Int64 {
	if f, ok := v[k].(float64); ok {
		return types.Int64Value(int64(f))
	}
	return types.Int64Null()
}

func jBool(v map[string]any, k string) types.Bool {
	b, _ := v[k].(bool)
	return types.BoolValue(b)
}

func jList(v map[string]any, k string, keepNull types.List) types.List {
	var out []string
	if xs, ok := v[k].([]any); ok {
		for _, x := range xs {
			if s, ok := x.(string); ok {
				out = append(out, s)
			}
		}
	}
	return listOf(out, keepNull)
}

// optional sets body[k] when v is known and set.
func setInt(body map[string]any, k string, v types.Int64) {
	if !v.IsNull() && !v.IsUnknown() {
		body[k] = v.ValueInt64()
	}
}

// roll_secrets_ca: a project's own certificate authority, limited to its domains.
type caModel struct {
	ID          types.String `tfsdk:"id"`
	Project     types.String `tfsdk:"project"`
	Name        types.String `tfsdk:"name"`
	Domains     types.List   `tfsdk:"domains"`
	AllowIP     types.Bool   `tfsdk:"allow_ip"`
	MaxDays     types.Int64  `tfsdk:"max_days"`
	Certificate types.String `tfsdk:"certificate"`
}

func newCaResource() resource.Resource {
	return &registration[caModel]{
		typeName: "_secrets_ca", route: "cas", what: "CA",
		description: "A project's own certificate authority (docs/secrets.md, \"Certificates\"): it signs short-lived certificates for its domains only, and its key never leaves roll. Link keys to it (`kind = \"pki\"`).",
		schema: map[string]schema.Attribute{
			"name":        reqString("The CA's name."),
			"domains":     strList(true, "DNS domains it signs for (a domain and every name under it)."),
			"allow_ip":    optBool("Whether it also signs IP addresses."),
			"max_days":    optInt("The longest a certificate lives, in days (default 90, 397 at most)."),
			"certificate": outString("The CA's certificate (PEM), for clients to trust."),
		},
		keys: func(m *caModel) (*types.String, *types.String) { return &m.Project, &m.ID },
		body: func(ctx context.Context, m *caModel, _ func(string) types.String, d *diag.Diagnostics) map[string]any {
			b := map[string]any{"name": m.Name.ValueString(), "domains": stringList(ctx, m.Domains, d), "allow_ip": m.AllowIP.ValueBool()}
			setInt(b, "max_days", m.MaxDays)
			return b
		},
		fill: func(m *caModel, v map[string]any) {
			m.ID, m.Name, m.Certificate, m.AllowIP, m.MaxDays = jStr(v, "id"), jStr(v, "name"), jStr(v, "certificate"), jBool(v, "allow_ip"), jInt(v, "max_days")
			m.Domains = jList(v, "domains", m.Domains)
		},
	}
}

// roll_secrets_ssh_ca: a project's SSH certificate authority for the logins (and hosts) it allows.
type sshCaModel struct {
	ID          types.String `tfsdk:"id"`
	Project     types.String `tfsdk:"project"`
	Name        types.String `tfsdk:"name"`
	Principals  types.List   `tfsdk:"principals"`
	MaxHours    types.Int64  `tfsdk:"max_hours"`
	HostDomains types.List   `tfsdk:"host_domains"`
	MaxHostDays types.Int64  `tfsdk:"max_host_days"`
	PublicKey   types.String `tfsdk:"public_key"`
	KnownHosts  types.String `tfsdk:"known_hosts"`
}

func newSshCaResource() resource.Resource {
	return &registration[sshCaModel]{
		typeName: "_secrets_ssh_ca", route: "ssh-cas", what: "SSH CA",
		description: "A project's SSH certificate authority (docs/secrets.md, \"SSH certificates\"): short-lived user certificates for its principals, and host certificates for its host domains. Servers trust public_key (TrustedUserCAKeys).",
		schema: map[string]schema.Attribute{
			"name":          reqString("The SSH CA's name."),
			"principals":    strList(true, "The logins (server user names) it signs for."),
			"max_hours":     optInt("The longest a user certificate lives, in hours (default 24, a week at most)."),
			"host_domains":  strList(false, "DNS domains whose hosts it signs host keys for (none: no hosts)."),
			"max_host_days": optInt("The longest a host certificate lives, in days (397 at most)."),
			"public_key":    outString("ssh-ed25519 …, for servers' TrustedUserCAKeys."),
			"known_hosts":   outString("The @cert-authority line that trusts the hosts it signs, when it signs hosts."),
		},
		keys: func(m *sshCaModel) (*types.String, *types.String) { return &m.Project, &m.ID },
		body: func(ctx context.Context, m *sshCaModel, _ func(string) types.String, d *diag.Diagnostics) map[string]any {
			b := map[string]any{"name": m.Name.ValueString(), "principals": stringList(ctx, m.Principals, d)}
			if hd := stringList(ctx, m.HostDomains, d); len(hd) > 0 {
				b["host_domains"] = hd
			}
			setInt(b, "max_hours", m.MaxHours)
			setInt(b, "max_host_days", m.MaxHostDays)
			return b
		},
		fill: func(m *sshCaModel, v map[string]any) {
			m.ID, m.Name, m.PublicKey, m.KnownHosts, m.MaxHours, m.MaxHostDays = jStr(v, "id"), jStr(v, "name"), jStr(v, "public_key"), jStr(v, "known_hosts"), jInt(v, "max_hours"), jInt(v, "max_host_days")
			m.Principals, m.HostDomains = jList(v, "principals", m.Principals), jList(v, "host_domains", m.HostDomains)
		},
	}
}

// roll_secrets_transit_key: a named encryption key that never leaves roll.
type transitKeyModel struct {
	ID            types.String `tfsdk:"id"`
	Project       types.String `tfsdk:"project"`
	Name          types.String `tfsdk:"name"`
	LatestVersion types.Int64  `tfsdk:"latest_version"`
}

func newTransitKeyResource() resource.Resource {
	return &registration[transitKeyModel]{
		typeName: "_secrets_transit_key", route: "transit", what: "encryption key",
		description: "An encryption key (docs/secrets.md, \"Encryption keys\"): encrypt, decrypt, sign and HMAC through roll; the key never leaves it. Destroying it destroys what it encrypted for good.",
		schema: map[string]schema.Attribute{
			"name":           reqString("The key's name (what clients call transit(name) with)."),
			"latest_version": schema.Int64Attribute{Computed: true, Description: "Its newest version (rotations add one).", PlanModifiers: []planmodifier.Int64{int64planmodifier.UseStateForUnknown()}},
		},
		keys: func(m *transitKeyModel) (*types.String, *types.String) { return &m.Project, &m.ID },
		body: func(_ context.Context, m *transitKeyModel, _ func(string) types.String, _ *diag.Diagnostics) map[string]any {
			return map[string]any{"name": m.Name.ValueString()}
		},
		fill: func(m *transitKeyModel, v map[string]any) {
			m.ID, m.Name, m.LatestVersion = jStr(v, "id"), jStr(v, "name"), jInt(v, "latest_version")
		},
		// By name, with the name as its confirmation (what it encrypted can't be read after).
		deletePath: func(m *transitKeyModel) string {
			return "transit/" + url.PathEscape(m.Name.ValueString()) + "?confirm=" + url.QueryEscape(m.Name.ValueString())
		},
	}
}

// roll_secrets_outside_database: a database roll doesn't run, whose logins roll rotates or issues.
type outsideDatabaseModel struct {
	ID                types.String `tfsdk:"id"`
	Project           types.String `tfsdk:"project"`
	Name              types.String `tfsdk:"name"`
	AdminURLWO        types.String `tfsdk:"admin_url_wo"`
	AdminURLWOVersion types.Int64  `tfsdk:"admin_url_wo_version"`
	Roles             types.List   `tfsdk:"roles"`
	Group             types.String `tfsdk:"group"`
	CA                types.String `tfsdk:"ca"`
	Host              types.String `tfsdk:"host"`
}

func newOutsideDatabaseResource() resource.Resource {
	return &registration[outsideDatabaseModel]{
		typeName: "_secrets_outside_database", route: "outside-databases", what: "database outside roll",
		description: "A database roll doesn't run (docs/secrets.md, \"Databases outside roll\": Postgres, MySQL, SQL Server, ClickHouse, RabbitMQ, Redis, MongoDB): two roles that take turns (managed links) and a group dynamic logins join. Its admin URL is write-only (Terraform 1.11+): sent once, sealed in roll, never in the state.",
		schema: map[string]schema.Attribute{
			"name": reqString("Its name."),
			"admin_url_wo": schema.StringAttribute{Required: true, Sensitive: true, WriteOnly: true,
				Description: "postgres://admin:…@host:5432/db (or mysql://, sqlserver://, clickhouse://, rabbitmq://, rediss://, mongodb+srv://): a user that may alter the roles and create logins. Write-only: change admin_url_wo_version to register it again."},
			"admin_url_wo_version": schema.Int64Attribute{Required: true, Description: "Bump it to register the database again with a new admin URL (it replaces the registration).",
				PlanModifiers: []planmodifier.Int64{int64planmodifier.RequiresReplace()}},
			"roles": strList(false, "Two login roles that take turns (managed links)."),
			"group": optString("A role each dynamic login joins (dynamic links)."),
			"ca":    optString("The server's CA certificates (PEM), when the system's don't cover it (Amazon RDS)."),
			"host":  outString("Where it is (the URL itself stays sealed in roll)."),
		},
		keys: func(m *outsideDatabaseModel) (*types.String, *types.String) { return &m.Project, &m.ID },
		body: func(ctx context.Context, m *outsideDatabaseModel, cfg func(string) types.String, d *diag.Diagnostics) map[string]any {
			b := map[string]any{"name": m.Name.ValueString(), "url": cfg("admin_url_wo").ValueString(), "roles": stringList(ctx, m.Roles, d)}
			if b["roles"] == nil {
				b["roles"] = []string{}
			}
			if s := m.Group.ValueString(); s != "" {
				b["group"] = s
			}
			if s := m.CA.ValueString(); s != "" {
				b["ca"] = s
			}
			return b
		},
		fill: func(m *outsideDatabaseModel, v map[string]any) {
			m.ID, m.Name, m.Host, m.AdminURLWO = jStr(v, "id"), jStr(v, "name"), jStr(v, "host"), types.StringNull()
			m.Roles = jList(v, "roles", m.Roles)
			if g := jStr(v, "group"); !g.IsNull() || !m.Group.IsNull() {
				m.Group = g
			}
			if c := jStr(v, "ca"); !c.IsNull() || !m.CA.IsNull() {
				m.CA = c
			}
		},
	}
}
