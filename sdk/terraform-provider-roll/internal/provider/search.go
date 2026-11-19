package provider

import (
	"context"
	"encoding/json"
	"fmt"
	"net/http"
	"net/netip"
	"net/url"
	"strings"

	"github.com/hashicorp/terraform-plugin-framework-validators/stringvalidator"
	"github.com/hashicorp/terraform-plugin-framework/attr"
	"github.com/hashicorp/terraform-plugin-framework/datasource"
	dschema "github.com/hashicorp/terraform-plugin-framework/datasource/schema"
	"github.com/hashicorp/terraform-plugin-framework/diag"
	"github.com/hashicorp/terraform-plugin-framework/path"
	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/boolplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/int64planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/listdefault"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/schema/validator"
	"github.com/hashicorp/terraform-plugin-framework/types"
)

// roll search (docs/search/README.md, "roll API"): clusters, their API keys and snapshot
// repositories, through /v1/search as `roll search` calls it.

func searchCluster(id string) string { return "/v1/search/clusters/" + url.PathEscape(id) }

// The API's cluster, as much of it as the provider keeps.
type searchClusterAnswer struct {
	ID                      string   `json:"id"`
	Name                    string   `json:"name"`
	Flavor                  string   `json:"flavor"`
	IdleTimeoutS            int64    `json:"idle_timeout_s"`
	MaxInFlight             int64    `json:"max_in_flight"`
	MaxBodyMB               int64    `json:"max_body_mb"`
	DestructiveRequiresName bool     `json:"destructive_requires_name"`
	IPAllowlist             []ipRule `json:"ip_allowlist"`
	URL                     string   `json:"url"`
	Username                string   `json:"username"`
	Password                string   `json:"password"`
	Status                  struct {
		State string `json:"state"`
	} `json:"status"`
}

type ipRule struct {
	CIDR        string `json:"cidr"`
	Description string `json:"description,omitempty"`
}

// searchInfo is GET /v1/search: the node's versions and the defaults a cluster gets.
type searchInfo struct {
	Version           string `json:"version"`
	OpenSearchVersion string `json:"opensearch_version"`
	Defaults          struct {
		IdleTimeoutS int64 `json:"idle_timeout_s"`
	} `json:"defaults"`
}

func (i *searchInfo) versionOf(flavor string) string {
	if flavor == "opensearch" {
		return i.OpenSearchVersion
	}
	return i.Version
}

// canonicalCIDR is how roll keeps an allowlist entry: an address is a /32 (or /128), a network
// has its host bits cleared. "" when it doesn't parse (roll answers 400 then).
func canonicalCIDR(s string) string {
	s = strings.TrimSpace(s)
	if p, err := netip.ParsePrefix(s); err == nil {
		return p.Masked().String()
	}
	if a, err := netip.ParseAddr(s); err == nil {
		return netip.PrefixFrom(a, a.BitLen()).String()
	}
	return ""
}

// searchWriteError reports a refused create or update. A 409 is past one of a person's caps
// (the message names the cap, the count and the flag that raises it) or a taken name.
func searchWriteError(d *diag.Diagnostics, what, name string, err error) {
	e, ok := err.(*apiError)
	switch {
	case ok && e.status == http.StatusConflict && strings.Contains(e.message, "a person may have"):
		d.AddError("past your roll search cap", e.message)
	case ok && e.status == http.StatusConflict:
		d.AddAttributeError(path.Root("name"), "search cluster "+name+" can't be made", e.message+". A deleted cluster frees its name at once; `roll search clusters restore` brings one back under another name.")
	default:
		d.AddError(what, err.Error())
	}
}

// ── roll_search_cluster ──

type searchClusterResource struct{ c *client }

type searchClusterModel struct {
	ID                      types.String `tfsdk:"id"`
	Name                    types.String `tfsdk:"name"`
	Flavor                  types.String `tfsdk:"flavor"`
	AlwaysOn                types.Bool   `tfsdk:"always_on"`
	IdleTimeoutS            types.Int64  `tfsdk:"idle_timeout_s"`
	MaxInFlight             types.Int64  `tfsdk:"max_in_flight"`
	MaxBodyMB               types.Int64  `tfsdk:"max_body_mb"`
	DestructiveRequiresName types.Bool   `tfsdk:"destructive_requires_name"`
	IPAllowlist             types.List   `tfsdk:"ip_allowlist"`
	Endpoint                types.String `tfsdk:"endpoint"`
	Username                types.String `tfsdk:"username"`
	Password                types.String `tfsdk:"password"`
	Status                  types.String `tfsdk:"status"`
	Version                 types.String `tfsdk:"version"`
}

type ipRuleModel struct {
	CIDR        types.String `tfsdk:"cidr"`
	Description types.String `tfsdk:"description"`
}

var ipRuleType = types.ObjectType{AttrTypes: map[string]attr.Type{"cidr": types.StringType, "description": types.StringType}}

func newSearchClusterResource() resource.Resource { return &searchClusterResource{} }

func (r *searchClusterResource) Metadata(_ context.Context, req resource.MetadataRequest, resp *resource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_search_cluster"
}

func (r *searchClusterResource) Schema(_ context.Context, _ resource.SchemaRequest, resp *resource.SchemaResponse) {
	keep := []planmodifier.String{stringplanmodifier.UseStateForUnknown()}
	keepInt := []planmodifier.Int64{int64planmodifier.UseStateForUnknown()}
	resp.Schema = schema.Schema{
		Description: "A roll search cluster: Elasticsearch or OpenSearch, scaled to zero when idle unless always on. Destroying it deletes it; it can be restored for the node's retention window (7 days by default) with `roll search clusters restore`.",
		Attributes: map[string]schema.Attribute{
			"id":   schema.StringAttribute{Computed: true, PlanModifiers: keep, Description: "srch_…"},
			"name": schema.StringAttribute{Required: true, PlanModifiers: []planmodifier.String{stringplanmodifier.RequiresReplace()}, Description: "1-63 of a-z, 0-9 and -, starting with a letter: the cluster's user and its host label. A new name is a new cluster."},
			"flavor": schema.StringAttribute{Optional: true, Computed: true, PlanModifiers: keep, Validators: []validator.String{stringvalidator.OneOf("elasticsearch", "opensearch")},
				Description: "elasticsearch or opensearch (the node's default when left out). A new flavor switches the cluster in place, keeping its data: it reloads as soon as no request runs. Keys don't carry over (an API key isn't an OpenSearch user): give roll_search_api_key `replace_triggered_by` on it."},
			"always_on": schema.BoolAttribute{Optional: true, Computed: true, Description: "Never unload (idle_timeout_s 0). A person may have 2 by default."},
			"idle_timeout_s": schema.Int64Attribute{Optional: true, Computed: true,
				Description: "Seconds without requests before it scales to zero; 0 is always on. The node's default when left out."},
			"max_in_flight":             schema.Int64Attribute{Optional: true, Computed: true, PlanModifiers: keepInt, Description: "Requests at once, 1-4096 (as many more queue; beyond that, 429)."},
			"max_body_mb":               schema.Int64Attribute{Optional: true, Computed: true, PlanModifiers: keepInt, Description: "http.max_content_length in MiB, 1-2047."},
			"destructive_requires_name": schema.BoolAttribute{Optional: true, Computed: true, PlanModifiers: []planmodifier.Bool{boolplanmodifier.UseStateForUnknown()}, Description: "The default of action.destructive_requires_name."},
			"ip_allowlist": schema.ListNestedAttribute{
				Optional: true, Computed: true, Default: listdefault.StaticValue(types.ListValueMust(ipRuleType, []attr.Value{})),
				Description: "Who may reach the endpoint (at most 64 entries); empty, the default, admits anyone. roll's API and the dashboard aren't filtered.",
				NestedObject: schema.NestedAttributeObject{Attributes: map[string]schema.Attribute{
					"cidr":        schema.StringAttribute{Required: true, Description: "A CIDR or an address, IPv4 or IPv6."},
					"description": schema.StringAttribute{Optional: true},
				}},
			},
			"endpoint": schema.StringAttribute{Computed: true, PlanModifiers: keep, Description: "Where clients connect."},
			"username": schema.StringAttribute{Computed: true, PlanModifiers: keep, Description: "The cluster's own user (its name)."},
			"password": schema.StringAttribute{Computed: true, Sensitive: true, PlanModifiers: keep, Description: "The cluster's own password, from when it was made (not after an import). Prefer roll_search_api_key for clients."},
			"status":   schema.StringAttribute{Computed: true, Description: "loaded or asleep, when last read."},
			"version":  schema.StringAttribute{Computed: true, PlanModifiers: keep, Description: "The Elasticsearch or OpenSearch version it answers as."},
		},
	}
}

func (r *searchClusterResource) Configure(_ context.Context, req resource.ConfigureRequest, _ *resource.ConfigureResponse) {
	if c, ok := req.ProviderData.(*client); ok {
		r.c = c
	}
}

// ModifyPlan keeps always_on and idle_timeout_s one setting: either decides the other, and
// turning always_on off without a timeout goes back to the node's default (known at apply).
func (r *searchClusterResource) ModifyPlan(ctx context.Context, req resource.ModifyPlanRequest, resp *resource.ModifyPlanResponse) {
	if req.Plan.Raw.IsNull() {
		return
	}
	var cfg, plan searchClusterModel
	resp.Diagnostics.Append(req.Config.Get(ctx, &cfg)...)
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	if resp.Diagnostics.HasError() {
		return
	}
	var state *searchClusterModel
	if !req.State.Raw.IsNull() {
		state = &searchClusterModel{}
		resp.Diagnostics.Append(req.State.Get(ctx, state)...)
	}
	// A flavor switch answers as the other product's version.
	if state != nil && !plan.Flavor.Equal(state.Flavor) {
		plan.Version = types.StringUnknown()
		resp.Diagnostics.Append(resp.Plan.SetAttribute(ctx, path.Root("version"), plan.Version)...)
	}
	ao, idle := cfg.AlwaysOn, cfg.IdleTimeoutS
	if ao.IsUnknown() || idle.IsUnknown() {
		return
	}
	switch {
	case !ao.IsNull() && !idle.IsNull() && ao.ValueBool() != (idle.ValueInt64() == 0):
		resp.Diagnostics.AddAttributeError(path.Root("idle_timeout_s"), "always_on and idle_timeout_s disagree", "idle_timeout_s 0 is always on: set one of them")
		return
	case idle.ValueInt64() < 0:
		resp.Diagnostics.AddAttributeError(path.Root("idle_timeout_s"), "a negative timeout", "idle_timeout_s is 0 (always on) or more")
		return
	case !idle.IsNull():
		plan.IdleTimeoutS, plan.AlwaysOn = idle, types.BoolValue(idle.ValueInt64() == 0)
	case ao.ValueBool():
		plan.IdleTimeoutS, plan.AlwaysOn = types.Int64Value(0), types.BoolValue(true)
	case !ao.IsNull():
		plan.AlwaysOn, plan.IdleTimeoutS = types.BoolValue(false), types.Int64Unknown()
		if state != nil && state.IdleTimeoutS.ValueInt64() > 0 {
			plan.IdleTimeoutS = state.IdleTimeoutS
		}
	case state != nil:
		plan.AlwaysOn, plan.IdleTimeoutS = state.AlwaysOn, state.IdleTimeoutS
	default:
		plan.AlwaysOn, plan.IdleTimeoutS = types.BoolUnknown(), types.Int64Unknown()
	}
	resp.Diagnostics.Append(resp.Plan.Set(ctx, plan)...)
}

func (m *searchClusterModel) rules(ctx context.Context, d *diag.Diagnostics) []ipRuleModel {
	var out []ipRuleModel
	if !m.IPAllowlist.IsNull() && !m.IPAllowlist.IsUnknown() {
		d.Append(m.IPAllowlist.ElementsAs(ctx, &out, false)...)
	}
	return out
}

// options is the create or update body: every setting the plan knows.
func (r *searchClusterResource) options(ctx context.Context, m *searchClusterModel, d *diag.Diagnostics) map[string]any {
	body := map[string]any{}
	if given(m.Flavor) {
		body["flavor"] = m.Flavor.ValueString()
	}
	switch {
	case givenInt(m.IdleTimeoutS):
		body["idle_timeout_s"] = m.IdleTimeoutS.ValueInt64()
	case !m.AlwaysOn.IsNull() && !m.AlwaysOn.IsUnknown() && !m.AlwaysOn.ValueBool():
		// Back from always on: the node's default, unless that is always on too.
		var info searchInfo
		if err := r.c.call(ctx, http.MethodGet, "/v1/search", nil, &info); err != nil {
			d.AddError("reading roll search's defaults", err.Error())
		} else if info.Defaults.IdleTimeoutS == 0 {
			d.AddAttributeError(path.Root("idle_timeout_s"), "which timeout?", "this node keeps clusters loaded by default: set idle_timeout_s above 0")
		} else {
			body["idle_timeout_s"] = info.Defaults.IdleTimeoutS
		}
	}
	if givenInt(m.MaxInFlight) {
		body["max_in_flight"] = m.MaxInFlight.ValueInt64()
	}
	if givenInt(m.MaxBodyMB) {
		body["max_body_mb"] = m.MaxBodyMB.ValueInt64()
	}
	if !m.DestructiveRequiresName.IsNull() && !m.DestructiveRequiresName.IsUnknown() {
		body["destructive_requires_name"] = m.DestructiveRequiresName.ValueBool()
	}
	if !m.IPAllowlist.IsUnknown() {
		allow := []ipRule{}
		for _, x := range m.rules(ctx, d) {
			allow = append(allow, ipRule{CIDR: x.CIDR.ValueString(), Description: x.Description.ValueString()})
		}
		body["ip_allowlist"] = allow
	}
	return body
}

// fill sets the model from the API's cluster. Allowlist entries keep the spelling they were
// written in (`10.1.2.3/8`) while roll keeps the same network canonical (`10.0.0.0/8`).
func (m *searchClusterModel) fill(ctx context.Context, s *searchClusterAnswer, info *searchInfo, d *diag.Diagnostics) {
	written := m.rules(ctx, d)
	rules := make([]attr.Value, 0, len(s.IPAllowlist))
	for i, x := range s.IPAllowlist {
		cidr, desc := types.StringValue(x.CIDR), optional(x.Description)
		if i < len(written) && canonicalCIDR(written[i].CIDR.ValueString()) == x.CIDR {
			cidr = written[i].CIDR
		}
		if i < len(written) && written[i].Description.ValueString() == x.Description {
			desc = written[i].Description
		}
		rules = append(rules, types.ObjectValueMust(ipRuleType.AttrTypes, map[string]attr.Value{"cidr": cidr, "description": desc}))
	}
	m.IPAllowlist = types.ListValueMust(ipRuleType, rules)
	m.ID, m.Name, m.Flavor = types.StringValue(s.ID), types.StringValue(s.Name), types.StringValue(s.Flavor)
	m.IdleTimeoutS, m.AlwaysOn = types.Int64Value(s.IdleTimeoutS), types.BoolValue(s.IdleTimeoutS == 0)
	m.MaxInFlight, m.MaxBodyMB = types.Int64Value(s.MaxInFlight), types.Int64Value(s.MaxBodyMB)
	m.DestructiveRequiresName = types.BoolValue(s.DestructiveRequiresName)
	m.Endpoint, m.Username, m.Status = types.StringValue(s.URL), types.StringValue(s.Username), types.StringValue(s.Status.State)
	if s.Password != "" {
		m.Password = types.StringValue(s.Password)
	} else if m.Password.IsUnknown() {
		m.Password = types.StringNull()
	}
	if info != nil {
		m.Version = types.StringValue(info.versionOf(s.Flavor))
	} else if m.Version.IsUnknown() {
		m.Version = types.StringNull()
	}
}

// info reads the node's versions; a failure only leaves version as it was.
func (r *searchClusterResource) info(ctx context.Context) *searchInfo {
	var info searchInfo
	if r.c.call(ctx, http.MethodGet, "/v1/search", nil, &info) != nil {
		return nil
	}
	return &info
}

func (r *searchClusterResource) Create(ctx context.Context, req resource.CreateRequest, resp *resource.CreateResponse) {
	var m searchClusterModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &m)...)
	if resp.Diagnostics.HasError() {
		return
	}
	body := r.options(ctx, &m, &resp.Diagnostics)
	if resp.Diagnostics.HasError() {
		return
	}
	body["name"] = m.Name.ValueString()
	var s searchClusterAnswer
	if err := r.c.call(ctx, http.MethodPost, "/v1/search/clusters", body, &s); err != nil {
		searchWriteError(&resp.Diagnostics, "making the search cluster", m.Name.ValueString(), err)
		return
	}
	m.fill(ctx, &s, r.info(ctx), &resp.Diagnostics)
	resp.Diagnostics.Append(resp.State.Set(ctx, m)...)
}

func (r *searchClusterResource) Read(ctx context.Context, req resource.ReadRequest, resp *resource.ReadResponse) {
	var m searchClusterModel
	resp.Diagnostics.Append(req.State.Get(ctx, &m)...)
	if resp.Diagnostics.HasError() {
		return
	}
	var s searchClusterAnswer
	if err := r.c.call(ctx, http.MethodGet, searchCluster(m.ID.ValueString()), nil, &s); notFound(err) {
		resp.State.RemoveResource(ctx)
		return
	} else if err != nil {
		resp.Diagnostics.AddError("reading the search cluster", err.Error())
		return
	}
	m.fill(ctx, &s, r.info(ctx), &resp.Diagnostics)
	resp.Diagnostics.Append(resp.State.Set(ctx, m)...)
}

func (r *searchClusterResource) Update(ctx context.Context, req resource.UpdateRequest, resp *resource.UpdateResponse) {
	var m, prior searchClusterModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &m)...)
	resp.Diagnostics.Append(req.State.Get(ctx, &prior)...)
	if resp.Diagnostics.HasError() {
		return
	}
	body := r.options(ctx, &m, &resp.Diagnostics)
	if resp.Diagnostics.HasError() {
		return
	}
	var s searchClusterAnswer
	if err := r.c.call(ctx, http.MethodPatch, searchCluster(prior.ID.ValueString()), body, &s); err != nil {
		searchWriteError(&resp.Diagnostics, "updating the search cluster", m.Name.ValueString(), err)
		return
	}
	m.Password = prior.Password
	m.fill(ctx, &s, r.info(ctx), &resp.Diagnostics)
	resp.Diagnostics.Append(resp.State.Set(ctx, m)...)
}

// Delete deletes the cluster: recoverable for the node's retention window (7 days by default).
func (r *searchClusterResource) Delete(ctx context.Context, req resource.DeleteRequest, resp *resource.DeleteResponse) {
	var m searchClusterModel
	resp.Diagnostics.Append(req.State.Get(ctx, &m)...)
	if err := r.c.call(ctx, http.MethodDelete, searchCluster(m.ID.ValueString()), nil, nil); err != nil && !notFound(err) {
		resp.Diagnostics.AddError("deleting the search cluster", err.Error())
	}
}

// ImportState takes the cluster's id or its name (the API reads either).
func (r *searchClusterResource) ImportState(ctx context.Context, req resource.ImportStateRequest, resp *resource.ImportStateResponse) {
	resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root("id"), req.ID)...)
}

// ── data "roll_search_cluster" ──

type searchClusterDataSource struct{ c *client }

type searchClusterDataModel struct {
	ID           types.String `tfsdk:"id"`
	Name         types.String `tfsdk:"name"`
	Flavor       types.String `tfsdk:"flavor"`
	AlwaysOn     types.Bool   `tfsdk:"always_on"`
	IdleTimeoutS types.Int64  `tfsdk:"idle_timeout_s"`
	IPAllowlist  types.List   `tfsdk:"ip_allowlist"`
	Endpoint     types.String `tfsdk:"endpoint"`
	Username     types.String `tfsdk:"username"`
	Status       types.String `tfsdk:"status"`
	Version      types.String `tfsdk:"version"`
}

func newSearchClusterDataSource() datasource.DataSource { return &searchClusterDataSource{} }

func (d *searchClusterDataSource) Metadata(_ context.Context, req datasource.MetadataRequest, resp *datasource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_search_cluster"
}

func (d *searchClusterDataSource) Schema(_ context.Context, _ datasource.SchemaRequest, resp *datasource.SchemaResponse) {
	resp.Schema = dschema.Schema{
		Description: "A roll search cluster made elsewhere (the dashboard, `roll search`, another configuration), by id or by name. Its password isn't read.",
		Attributes: map[string]dschema.Attribute{
			"id":             dschema.StringAttribute{Optional: true, Computed: true, Description: "srch_…; or give name."},
			"name":           dschema.StringAttribute{Optional: true, Computed: true},
			"flavor":         dschema.StringAttribute{Computed: true},
			"always_on":      dschema.BoolAttribute{Computed: true},
			"idle_timeout_s": dschema.Int64Attribute{Computed: true},
			"ip_allowlist": dschema.ListNestedAttribute{Computed: true, NestedObject: dschema.NestedAttributeObject{Attributes: map[string]dschema.Attribute{
				"cidr":        dschema.StringAttribute{Computed: true},
				"description": dschema.StringAttribute{Computed: true},
			}}},
			"endpoint": dschema.StringAttribute{Computed: true},
			"username": dschema.StringAttribute{Computed: true},
			"status":   dschema.StringAttribute{Computed: true},
			"version":  dschema.StringAttribute{Computed: true},
		},
	}
}

func (d *searchClusterDataSource) Configure(_ context.Context, req datasource.ConfigureRequest, _ *datasource.ConfigureResponse) {
	d.c, _ = req.ProviderData.(*client)
}

func (d *searchClusterDataSource) Read(ctx context.Context, req datasource.ReadRequest, resp *datasource.ReadResponse) {
	var m searchClusterDataModel
	resp.Diagnostics.Append(req.Config.Get(ctx, &m)...)
	if resp.Diagnostics.HasError() {
		return
	}
	if given(m.ID) == given(m.Name) {
		resp.Diagnostics.AddError("which cluster", "give id or name, not both")
		return
	}
	key := m.ID.ValueString()
	if !given(m.ID) {
		key = m.Name.ValueString()
	}
	var s searchClusterAnswer
	if err := d.c.call(ctx, http.MethodGet, searchCluster(key), nil, &s); err != nil {
		resp.Diagnostics.AddError("reading the search cluster", err.Error())
		return
	}
	var info searchInfo
	if err := d.c.call(ctx, http.MethodGet, "/v1/search", nil, &info); err != nil {
		resp.Diagnostics.AddError("reading roll search's versions", err.Error())
		return
	}
	var full searchClusterModel
	full.IPAllowlist = types.ListNull(ipRuleType)
	full.fill(ctx, &s, &info, &resp.Diagnostics)
	m.ID, m.Name, m.Flavor, m.AlwaysOn, m.IdleTimeoutS = full.ID, full.Name, full.Flavor, full.AlwaysOn, full.IdleTimeoutS
	m.IPAllowlist, m.Endpoint, m.Username, m.Status, m.Version = full.IPAllowlist, full.Endpoint, full.Username, full.Status, full.Version
	resp.Diagnostics.Append(resp.State.Set(ctx, m)...)
}

// ── the console: one Elasticsearch request through roll's API ──

// console sends method and path to the cluster through POST …/request (as `roll search request`
// does), as the cluster's own user, and answers Elasticsearch's status and body. It wakes a
// sleeping cluster.
func (c *client) console(ctx context.Context, cluster, method, esPath string, body any) (int, []byte, error) {
	req := map[string]any{"method": method, "path": esPath}
	if body != nil {
		req["body"] = body
	}
	var out struct {
		Status int    `json:"status"`
		Body   string `json:"body"`
	}
	if err := c.call(ctx, http.MethodPost, searchCluster(cluster)+"/request", req, &out); err != nil {
		return 0, nil, err
	}
	return out.Status, []byte(out.Body), nil
}

// esReason is an Elasticsearch error's reason (never its whole body).
func esReason(status int, body []byte) string {
	var e struct {
		Error json.RawMessage `json:"error"`
	}
	_ = json.Unmarshal(body, &e)
	var obj struct {
		Type   string `json:"type"`
		Reason string `json:"reason"`
	}
	var s string
	switch {
	case json.Unmarshal(e.Error, &obj) == nil && obj.Reason != "":
		return fmt.Sprintf("%s: %s (%d)", obj.Type, obj.Reason, status)
	case json.Unmarshal(e.Error, &s) == nil && s != "":
		return fmt.Sprintf("%s (%d)", s, status)
	}
	return fmt.Sprintf("the cluster answered %d", status)
}
