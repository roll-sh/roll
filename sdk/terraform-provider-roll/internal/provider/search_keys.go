package provider

import (
	"context"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"net/http"
	"net/url"
	"regexp"
	"strconv"
	"strings"
	"time"

	"github.com/hashicorp/terraform-plugin-framework-validators/stringvalidator"
	"github.com/hashicorp/terraform-plugin-framework/diag"
	"github.com/hashicorp/terraform-plugin-framework/ephemeral"
	eschema "github.com/hashicorp/terraform-plugin-framework/ephemeral/schema"
	"github.com/hashicorp/terraform-plugin-framework/path"
	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/schema/validator"
	"github.com/hashicorp/terraform-plugin-framework/types"
)

// ── API keys: roll_search_api_key and its ephemeral twin ──

// keySpec is what a key is made with, from a resource's plan or an ephemeral's config.
type keySpec struct {
	Cluster, Name, Access, RoleDescriptors, Expiration types.String
}

// keyMade is a new key: `Encoded` for Elasticsearch (`Authorization: ApiKey …`), a user and
// password for OpenSearch, whose clients sign in with a user.
type keyMade struct {
	ID       string `json:"id"`
	Encoded  string `json:"api_key"`
	Username string `json:"username"`
	Password string `json:"password"`
	URL      string `json:"url"`
	Expires  *int64 `json:"expires_at"`
	// The cluster's flavor when it was made: a flavor switch doesn't carry keys across.
	Flavor string `json:"-"`
}

var expirationRe = regexp.MustCompile(`^([1-9][0-9]*)(d|h|m|s)$`)

// expirationSeconds reads an Elasticsearch time value in whole days, hours, minutes or seconds.
func expirationSeconds(v string) (int64, bool) {
	m := expirationRe.FindStringSubmatch(v)
	if m == nil {
		return 0, false
	}
	n, err := strconv.ParseInt(m[1], 10, 64)
	if err != nil || n > 1<<40 {
		return 0, false
	}
	return n * map[string]int64{"d": 86400, "h": 3600, "m": 60, "s": 1}[m[2]], true
}

func (k *keySpec) validate(d *diag.Diagnostics) {
	if given(k.Expiration) {
		if _, ok := expirationSeconds(k.Expiration.ValueString()); !ok {
			d.AddAttributeError(path.Root("expiration"), "expiration", "a whole number of days, hours, minutes or seconds: 30d, 12h, 90m, 3600s")
		}
	}
	if given(k.RoleDescriptors) {
		var obj map[string]json.RawMessage
		if json.Unmarshal([]byte(k.RoleDescriptors.ValueString()), &obj) != nil {
			d.AddAttributeError(path.Root("role_descriptors"), "role_descriptors", "a JSON object of role names to Elasticsearch role descriptors (jsonencode({...}))")
		}
		if given(k.Access) {
			d.AddAttributeError(path.Root("access"), "access or role_descriptors", "role_descriptors say what the key may do: leave access out")
		}
	}
}

// makeKey makes the key. Without role_descriptors through roll's keys route (both flavors);
// with them through the cluster's own `_security/api_key` (Elasticsearch only), encoded the way
// roll encodes its keys: base64(`<id>@<cluster>:<key>`), which names the cluster on any host.
func (c *client) makeKey(ctx context.Context, k *keySpec) (*keyMade, error) {
	var cl searchClusterAnswer
	if err := c.call(ctx, http.MethodGet, searchCluster(k.Cluster.ValueString()), nil, &cl); err != nil {
		return nil, err
	}
	var secs *int64
	if given(k.Expiration) {
		s, _ := expirationSeconds(k.Expiration.ValueString())
		secs = &s
	}
	if !given(k.RoleDescriptors) {
		access := "data"
		if given(k.Access) {
			access = k.Access.ValueString()
		}
		var made keyMade
		body := map[string]any{"name": k.Name.ValueString(), "access": access}
		if secs != nil {
			body["expires_in_s"] = *secs
		}
		if err := c.call(ctx, http.MethodPost, searchCluster(cl.ID)+"/keys", body, &made); err != nil {
			return nil, err
		}
		made.Flavor = cl.Flavor
		return &made, nil
	}
	if cl.Flavor == "opensearch" {
		return nil, fmt.Errorf("cluster %s is OpenSearch, which has no API keys with role descriptors: give access instead", cl.Name)
	}
	body := map[string]any{"name": k.Name.ValueString(), "role_descriptors": json.RawMessage(k.RoleDescriptors.ValueString())}
	if secs != nil {
		body["expiration"] = fmt.Sprintf("%ds", *secs)
	}
	status, raw, err := c.console(ctx, cl.ID, http.MethodPost, "/_security/api_key", body)
	if err != nil {
		return nil, err
	}
	if status != http.StatusOK {
		return nil, fmt.Errorf("the cluster refused the key: %s", esReason(status, raw))
	}
	var v struct {
		ID         string `json:"id"`
		Key        string `json:"api_key"`
		Expiration *int64 `json:"expiration"`
	}
	if json.Unmarshal(raw, &v) != nil || v.ID == "" || v.Key == "" {
		return nil, fmt.Errorf("the cluster answered without a key (%d)", status)
	}
	made := &keyMade{ID: v.ID, Encoded: base64.StdEncoding.EncodeToString([]byte(v.ID + "@" + cl.Name + ":" + v.Key)), URL: cl.URL, Flavor: cl.Flavor}
	if v.Expiration != nil {
		s := *v.Expiration / 1000
		made.Expires = &s
	}
	return made, nil
}

func (c *client) revokeKey(ctx context.Context, cluster, id string) error {
	err := c.call(ctx, http.MethodDelete, searchCluster(cluster)+"/keys/"+url.PathEscape(id), nil, nil)
	if notFound(err) {
		return nil
	}
	return err
}

func expiresAt(made *keyMade) types.String {
	if made.Expires == nil {
		return types.StringNull()
	}
	return types.StringValue(time.Unix(*made.Expires, 0).UTC().Format(time.RFC3339))
}

type searchKeyResource struct{ c *client }

type searchKeyModel struct {
	ID              types.String `tfsdk:"id"`
	Cluster         types.String `tfsdk:"cluster"`
	Name            types.String `tfsdk:"name"`
	Access          types.String `tfsdk:"access"`
	RoleDescriptors types.String `tfsdk:"role_descriptors"`
	Expiration      types.String `tfsdk:"expiration"`
	Encoded         types.String `tfsdk:"encoded"`
	Username        types.String `tfsdk:"username"`
	Password        types.String `tfsdk:"password"`
	URL             types.String `tfsdk:"url"`
	ExpiresAt       types.String `tfsdk:"expires_at"`
	Flavor          types.String `tfsdk:"flavor"`
}

func (m *searchKeyModel) spec() *keySpec {
	return &keySpec{Cluster: m.Cluster, Name: m.Name, Access: m.Access, RoleDescriptors: m.RoleDescriptors, Expiration: m.Expiration}
}

func (m *searchKeyModel) fill(made *keyMade) {
	m.ID, m.URL, m.ExpiresAt, m.Flavor = types.StringValue(made.ID), types.StringValue(made.URL), expiresAt(made), types.StringValue(made.Flavor)
	m.Encoded, m.Username, m.Password = optional(made.Encoded), optional(made.Username), optional(made.Password)
}

func newSearchKeyResource() resource.Resource { return &searchKeyResource{} }

func (r *searchKeyResource) Metadata(_ context.Context, req resource.MetadataRequest, resp *resource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_search_api_key"
}

// keyDescriptions describe what both the resource and the ephemeral have.
func keyDescriptions() map[string]string {
	return map[string]string{
		"cluster":          "The cluster's id (srch_…) or name.",
		"name":             "The key's name, as the cluster lists it.",
		"access":           "data (the default: indices, templates, pipelines and lifecycle policies, no security) or admin (everything the cluster's own user may do). Not with role_descriptors.",
		"role_descriptors": "Elasticsearch role descriptors, as JSON (jsonencode({...})): what the key may do, as `_security/api_key` takes them. Elasticsearch clusters only.",
		"expiration":       "When it stops working, from when it's made: 30d, 12h, 90m or 3600s. Never when left out.",
		"encoded":          "The key, for `Authorization: ApiKey …` (Elasticsearch clusters). Shown once, when it's made.",
		"username":         "An OpenSearch cluster's key is a user of its own: its name (`<id>@<cluster>`).",
		"password":         "The OpenSearch key user's password. Shown once, when it's made.",
	}
}

func (r *searchKeyResource) Schema(_ context.Context, _ resource.SchemaRequest, resp *resource.SchemaResponse) {
	d := keyDescriptions()
	replace := []planmodifier.String{stringplanmodifier.RequiresReplace()}
	keep := []planmodifier.String{stringplanmodifier.UseStateForUnknown()}
	arg := func(required bool, key string, v ...validator.String) schema.StringAttribute {
		return schema.StringAttribute{Required: required, Optional: !required, PlanModifiers: replace, Description: d[key], Validators: v}
	}
	out := func(sensitive bool, key string) schema.StringAttribute {
		return schema.StringAttribute{Computed: true, Sensitive: sensitive, PlanModifiers: keep, Description: d[key]}
	}
	resp.Schema = schema.Schema{
		Description: "An API key of a roll search cluster, for a client that mustn't share the cluster's password. Its secret is in the state (sensitive): keep the state as private as the key, or use `ephemeral \"roll_search_api_key\"`. Every argument replaces it; destroying it revokes it.",
		Attributes: map[string]schema.Attribute{
			"id":               schema.StringAttribute{Computed: true, PlanModifiers: keep, Description: "The key's id on the cluster."},
			"cluster":          arg(true, "cluster"),
			"name":             arg(true, "name"),
			"access":           arg(false, "access", stringvalidator.OneOf("data", "admin")),
			"role_descriptors": arg(false, "role_descriptors"),
			"expiration":       arg(false, "expiration"),
			"encoded":          out(true, "encoded"),
			"username":         out(false, "username"),
			"password":         out(true, "password"),
			"url":              out(false, "url"),
			"expires_at":       schema.StringAttribute{Computed: true, PlanModifiers: keep, Description: "When it stops working (RFC 3339), if it does."},
			"flavor":           schema.StringAttribute{Computed: true, PlanModifiers: keep, Description: "The cluster's flavor when the key was made. After a flavor switch the key is gone (it doesn't carry over) and is made again."},
		},
	}
}

func (r *searchKeyResource) Configure(_ context.Context, req resource.ConfigureRequest, _ *resource.ConfigureResponse) {
	if c, ok := req.ProviderData.(*client); ok {
		r.c = c
	}
}

func (r *searchKeyResource) ValidateConfig(ctx context.Context, req resource.ValidateConfigRequest, resp *resource.ValidateConfigResponse) {
	var m searchKeyModel
	resp.Diagnostics.Append(req.Config.Get(ctx, &m)...)
	m.spec().validate(&resp.Diagnostics)
}

func (r *searchKeyResource) Create(ctx context.Context, req resource.CreateRequest, resp *resource.CreateResponse) {
	var m searchKeyModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &m)...)
	if resp.Diagnostics.HasError() {
		return
	}
	made, err := r.c.makeKey(ctx, m.spec())
	if err != nil {
		resp.Diagnostics.AddError("making the API key", err.Error())
		return
	}
	m.fill(made)
	resp.Diagnostics.Append(resp.State.Set(ctx, m)...)
}

// Read never wakes a sleeping cluster for a key: it is gone with its cluster, past its expiry
// or after a flavor switch, and on a loaded Elasticsearch cluster it is looked up (revoked
// elsewhere: gone).
func (r *searchKeyResource) Read(ctx context.Context, req resource.ReadRequest, resp *resource.ReadResponse) {
	var m searchKeyModel
	resp.Diagnostics.Append(req.State.Get(ctx, &m)...)
	if resp.Diagnostics.HasError() {
		return
	}
	var cl searchClusterAnswer
	if err := r.c.call(ctx, http.MethodGet, searchCluster(m.Cluster.ValueString()), nil, &cl); notFound(err) {
		resp.State.RemoveResource(ctx)
		return
	} else if err != nil {
		resp.Diagnostics.AddError("reading the key's cluster", err.Error())
		return
	}
	if given(m.Flavor) && m.Flavor.ValueString() != cl.Flavor {
		resp.State.RemoveResource(ctx)
		return
	}
	if given(m.ExpiresAt) {
		if at, err := time.Parse(time.RFC3339, m.ExpiresAt.ValueString()); err == nil && !at.After(time.Now()) {
			resp.State.RemoveResource(ctx)
			return
		}
	}
	if cl.Flavor != "elasticsearch" || cl.Status.State != "loaded" {
		return
	}
	status, raw, err := r.c.console(ctx, cl.ID, http.MethodGet, "/_security/api_key?id="+url.QueryEscape(m.ID.ValueString()), nil)
	if e, ok := err.(*apiError); ok && e.status == http.StatusForbidden {
		// A token that may make keys but not use the console (admin): nothing to compare.
		return
	} else if err != nil {
		resp.Diagnostics.AddError("looking the key up", err.Error())
		return
	}
	var v struct {
		Keys []struct {
			Invalidated bool `json:"invalidated"`
		} `json:"api_keys"`
	}
	_ = json.Unmarshal(raw, &v)
	if status == http.StatusNotFound || (status == http.StatusOK && (len(v.Keys) == 0 || v.Keys[0].Invalidated)) {
		resp.State.RemoveResource(ctx)
	}
}

func (r *searchKeyResource) Update(_ context.Context, _ resource.UpdateRequest, resp *resource.UpdateResponse) {
	resp.Diagnostics.AddError("API keys don't change in place", "every argument replaces the key")
}

func (r *searchKeyResource) Delete(ctx context.Context, req resource.DeleteRequest, resp *resource.DeleteResponse) {
	var m searchKeyModel
	resp.Diagnostics.Append(req.State.Get(ctx, &m)...)
	if resp.Diagnostics.HasError() {
		return
	}
	if err := r.c.revokeKey(ctx, m.Cluster.ValueString(), m.ID.ValueString()); err != nil {
		resp.Diagnostics.AddError("revoking the API key", err.Error())
	}
}

// ephemeral "roll_search_api_key": a key for this run only (Terraform 1.10+), revoked when the
// run closes it, never in the plan or the state: for a write-only argument (roll_secret's
// value_wo) or another provider's configuration.
type searchKeyEphemeral struct{ c *client }

type searchKeyEphemeralModel struct {
	Cluster         types.String `tfsdk:"cluster"`
	Name            types.String `tfsdk:"name"`
	Access          types.String `tfsdk:"access"`
	RoleDescriptors types.String `tfsdk:"role_descriptors"`
	Expiration      types.String `tfsdk:"expiration"`
	ID              types.String `tfsdk:"id"`
	Encoded         types.String `tfsdk:"encoded"`
	Username        types.String `tfsdk:"username"`
	Password        types.String `tfsdk:"password"`
	URL             types.String `tfsdk:"url"`
}

// openKey is what Close revokes.
type openKey struct {
	Cluster string `json:"cluster"`
	ID      string `json:"id"`
}

const searchKeyPrivate = "search_key"

func newSearchKeyEphemeral() ephemeral.EphemeralResource { return &searchKeyEphemeral{} }

func (e *searchKeyEphemeral) Metadata(_ context.Context, req ephemeral.MetadataRequest, resp *ephemeral.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_search_api_key"
}

func (e *searchKeyEphemeral) Schema(_ context.Context, _ ephemeral.SchemaRequest, resp *ephemeral.SchemaResponse) {
	d := keyDescriptions()
	arg := func(required bool, key string, v ...validator.String) eschema.StringAttribute {
		return eschema.StringAttribute{Required: required, Optional: !required, Description: d[key], Validators: v}
	}
	out := func(sensitive bool, key string) eschema.StringAttribute {
		return eschema.StringAttribute{Computed: true, Sensitive: sensitive, Description: d[key]}
	}
	resp.Schema = eschema.Schema{
		Description: "An API key of a roll search cluster for this run only: made when the run opens it, revoked when it closes, never written to the plan or the state.",
		Attributes: map[string]eschema.Attribute{
			"cluster":          arg(true, "cluster"),
			"name":             arg(true, "name"),
			"access":           arg(false, "access", stringvalidator.OneOf("data", "admin")),
			"role_descriptors": arg(false, "role_descriptors"),
			"expiration":       arg(false, "expiration"),
			"id":               eschema.StringAttribute{Computed: true},
			"encoded":          out(true, "encoded"),
			"username":         out(false, "username"),
			"password":         out(true, "password"),
			"url":              eschema.StringAttribute{Computed: true},
		},
	}
}

func (e *searchKeyEphemeral) Configure(_ context.Context, req ephemeral.ConfigureRequest, _ *ephemeral.ConfigureResponse) {
	if c, ok := req.ProviderData.(*client); ok {
		e.c = c
	}
}

func (e *searchKeyEphemeral) Open(ctx context.Context, req ephemeral.OpenRequest, resp *ephemeral.OpenResponse) {
	var m searchKeyEphemeralModel
	resp.Diagnostics.Append(req.Config.Get(ctx, &m)...)
	spec := &keySpec{Cluster: m.Cluster, Name: m.Name, Access: m.Access, RoleDescriptors: m.RoleDescriptors, Expiration: m.Expiration}
	spec.validate(&resp.Diagnostics)
	if resp.Diagnostics.HasError() {
		return
	}
	made, err := e.c.makeKey(ctx, spec)
	if err != nil {
		resp.Diagnostics.AddError("making the API key", err.Error())
		return
	}
	b, _ := json.Marshal(openKey{Cluster: m.Cluster.ValueString(), ID: made.ID})
	resp.Diagnostics.Append(resp.Private.SetKey(ctx, searchKeyPrivate, b)...)
	m.ID, m.URL = types.StringValue(made.ID), types.StringValue(made.URL)
	m.Encoded, m.Username, m.Password = optional(made.Encoded), optional(made.Username), optional(made.Password)
	resp.Diagnostics.Append(resp.Result.Set(ctx, m)...)
}

func (e *searchKeyEphemeral) Close(ctx context.Context, req ephemeral.CloseRequest, resp *ephemeral.CloseResponse) {
	b, diags := req.Private.GetKey(ctx, searchKeyPrivate)
	resp.Diagnostics.Append(diags...)
	var k openKey
	if len(b) == 0 || json.Unmarshal(b, &k) != nil {
		return
	}
	if err := e.c.revokeKey(ctx, k.Cluster, k.ID); err != nil {
		resp.Diagnostics.AddWarning("revoking the run's API key", "key "+k.ID+": "+err.Error()+" (revoke it with `roll search keys delete`)")
	}
}

// ── roll_search_snapshot_repository ──

// The settings each type's credentials go in. roll moves them out of the settings into the
// repository's sealed secrets at once and never returns them (docs/search/architecture.md,
// "Credentials").
var repoCredentials = map[string][]string{
	"s3":    {"access_key", "secret_key", "session_token"},
	"gcs":   {"credentials_file"},
	"azure": {"account", "key", "sas_token"},
}

type searchRepoResource struct{ c *client }

type searchRepoModel struct {
	ID          types.String `tfsdk:"id"`
	Cluster     types.String `tfsdk:"cluster"`
	Name        types.String `tfsdk:"name"`
	Type        types.String `tfsdk:"type"`
	Settings    types.Map    `tfsdk:"settings"`
	Credentials types.Map    `tfsdk:"credentials"`
}

func newSearchRepoResource() resource.Resource { return &searchRepoResource{} }

func (r *searchRepoResource) Metadata(_ context.Context, req resource.MetadataRequest, resp *resource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_search_snapshot_repository"
}

func (r *searchRepoResource) Schema(_ context.Context, _ resource.SchemaRequest, resp *resource.SchemaResponse) {
	replace := []planmodifier.String{stringplanmodifier.RequiresReplace()}
	resp.Schema = schema.Schema{
		Description: "A snapshot repository of a roll search cluster (`PUT _snapshot/<name>`): an S3 bucket, a Google Cloud Storage bucket or an Azure container that holds its snapshots. Destroying it unregisters it; the snapshots stay in the bucket.",
		Attributes: map[string]schema.Attribute{
			"id":      schema.StringAttribute{Computed: true, PlanModifiers: []planmodifier.String{stringplanmodifier.UseStateForUnknown()}, Description: "<cluster id>/<name>"},
			"cluster": schema.StringAttribute{Required: true, PlanModifiers: replace, Description: "The cluster's id (srch_…)."},
			"name":    schema.StringAttribute{Required: true, PlanModifiers: replace},
			"type":    schema.StringAttribute{Required: true, PlanModifiers: replace, Validators: []validator.String{stringvalidator.OneOf("s3", "gcs", "azure")}},
			"settings": schema.MapAttribute{Optional: true, ElementType: types.StringType,
				Description: "The repository's settings as Elasticsearch takes them (`bucket`, `base_path`, `endpoint`, `container`, `compress`, …), as strings. Credentials go in credentials."},
			"credentials": schema.MapAttribute{Optional: true, Sensitive: true, ElementType: types.StringType,
				Description: "s3: access_key, secret_key, session_token; gcs: credentials_file (the service account key file's JSON); azure: account, key, sas_token. Sealed by roll and never read back: a change made elsewhere isn't seen. Without them requests are anonymous."},
		},
	}
}

func (r *searchRepoResource) Configure(_ context.Context, req resource.ConfigureRequest, _ *resource.ConfigureResponse) {
	if c, ok := req.ProviderData.(*client); ok {
		r.c = c
	}
}

func (r *searchRepoResource) ValidateConfig(ctx context.Context, req resource.ValidateConfigRequest, resp *resource.ValidateConfigResponse) {
	var m searchRepoModel
	resp.Diagnostics.Append(req.Config.Get(ctx, &m)...)
	if resp.Diagnostics.HasError() || !given(m.Type) {
		return
	}
	allowed := repoCredentials[m.Type.ValueString()]
	if !m.Credentials.IsUnknown() {
		for k := range m.Credentials.Elements() {
			if !contains(allowed, k) {
				resp.Diagnostics.AddAttributeError(path.Root("credentials"), "credentials", fmt.Sprintf("a %s repository's credentials are %s, not %q", m.Type.ValueString(), strings.Join(allowed, ", "), k))
			}
		}
	}
	if !m.Settings.IsUnknown() {
		for k := range m.Settings.Elements() {
			for _, all := range repoCredentials {
				if contains(all, k) {
					resp.Diagnostics.AddAttributeError(path.Root("settings"), "a credential in settings", fmt.Sprintf("%q is a credential: put it in credentials, which are sensitive", k))
				}
			}
		}
	}
}

func contains(list []string, s string) bool {
	for _, x := range list {
		if x == s {
			return true
		}
	}
	return false
}

func repoPath(name string) string { return "/_snapshot/" + url.PathEscape(name) }

func (r *searchRepoResource) put(ctx context.Context, m *searchRepoModel, d *diag.Diagnostics) {
	settings := map[string]string{}
	for _, from := range []types.Map{m.Settings, m.Credentials} {
		if !from.IsNull() && !from.IsUnknown() {
			var kv map[string]string
			d.Append(from.ElementsAs(ctx, &kv, false)...)
			for k, v := range kv {
				settings[k] = v
			}
		}
	}
	if d.HasError() {
		return
	}
	status, raw, err := r.c.console(ctx, m.Cluster.ValueString(), http.MethodPut, repoPath(m.Name.ValueString()), map[string]any{"type": m.Type.ValueString(), "settings": settings})
	if err != nil {
		d.AddError("registering the snapshot repository", err.Error())
		return
	}
	if status != http.StatusOK {
		d.AddError("registering the snapshot repository", esReason(status, raw))
		return
	}
	m.ID = types.StringValue(m.Cluster.ValueString() + "/" + m.Name.ValueString())
}

func (r *searchRepoResource) Create(ctx context.Context, req resource.CreateRequest, resp *resource.CreateResponse) {
	var m searchRepoModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &m)...)
	if resp.Diagnostics.HasError() {
		return
	}
	if r.put(ctx, &m, &resp.Diagnostics); resp.Diagnostics.HasError() {
		return
	}
	resp.Diagnostics.Append(resp.State.Set(ctx, m)...)
}

// Read reads the repository back from a loaded cluster (or after an import), never waking a
// sleeping one for it: only the settings the state has are compared, as roll may add its own.
func (r *searchRepoResource) Read(ctx context.Context, req resource.ReadRequest, resp *resource.ReadResponse) {
	var m searchRepoModel
	resp.Diagnostics.Append(req.State.Get(ctx, &m)...)
	if resp.Diagnostics.HasError() {
		return
	}
	var cl searchClusterAnswer
	if err := r.c.call(ctx, http.MethodGet, searchCluster(m.Cluster.ValueString()), nil, &cl); notFound(err) {
		resp.State.RemoveResource(ctx)
		return
	} else if err != nil {
		resp.Diagnostics.AddError("reading the repository's cluster", err.Error())
		return
	}
	imported := m.Type.IsNull()
	if cl.Status.State != "loaded" && !imported {
		return
	}
	status, raw, err := r.c.console(ctx, cl.ID, http.MethodGet, repoPath(m.Name.ValueString()), nil)
	if err != nil {
		resp.Diagnostics.AddError("reading the snapshot repository", err.Error())
		return
	}
	if status == http.StatusNotFound {
		resp.State.RemoveResource(ctx)
		return
	}
	if status != http.StatusOK {
		resp.Diagnostics.AddError("reading the snapshot repository", esReason(status, raw))
		return
	}
	var all map[string]struct {
		Type     string                     `json:"type"`
		Settings map[string]json.RawMessage `json:"settings"`
	}
	_ = json.Unmarshal(raw, &all)
	repo, ok := all[m.Name.ValueString()]
	if !ok {
		resp.State.RemoveResource(ctx)
		return
	}
	read := map[string]string{}
	for k, v := range repo.Settings {
		var s string
		if json.Unmarshal(v, &s) != nil {
			s = string(v)
		}
		read[k] = s
	}
	settings := read
	if !imported {
		settings = map[string]string{}
		var had map[string]string
		if !m.Settings.IsNull() {
			resp.Diagnostics.Append(m.Settings.ElementsAs(ctx, &had, false)...)
		}
		for k := range had {
			if v, ok := read[k]; ok {
				settings[k] = v
			}
		}
	}
	m.Type = types.StringValue(repo.Type)
	if len(settings) > 0 || !m.Settings.IsNull() {
		v, diags := types.MapValueFrom(ctx, types.StringType, settings)
		resp.Diagnostics.Append(diags...)
		m.Settings = v
	}
	resp.Diagnostics.Append(resp.State.Set(ctx, m)...)
}

func (r *searchRepoResource) Update(ctx context.Context, req resource.UpdateRequest, resp *resource.UpdateResponse) {
	var m searchRepoModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &m)...)
	if resp.Diagnostics.HasError() {
		return
	}
	if r.put(ctx, &m, &resp.Diagnostics); resp.Diagnostics.HasError() {
		return
	}
	resp.Diagnostics.Append(resp.State.Set(ctx, m)...)
}

func (r *searchRepoResource) Delete(ctx context.Context, req resource.DeleteRequest, resp *resource.DeleteResponse) {
	var m searchRepoModel
	resp.Diagnostics.Append(req.State.Get(ctx, &m)...)
	status, raw, err := r.c.console(ctx, m.Cluster.ValueString(), http.MethodDelete, repoPath(m.Name.ValueString()), nil)
	switch {
	case notFound(err):
	case err != nil:
		resp.Diagnostics.AddError("unregistering the snapshot repository", err.Error())
	case status != http.StatusOK && status != http.StatusNotFound:
		resp.Diagnostics.AddError("unregistering the snapshot repository", esReason(status, raw))
	}
}

// ImportState takes `<cluster id>/<name>`. Credentials aren't read back: set them in the
// configuration, and the first apply registers the repository with them again.
func (r *searchRepoResource) ImportState(ctx context.Context, req resource.ImportStateRequest, resp *resource.ImportStateResponse) {
	cluster, name, ok := strings.Cut(req.ID, "/")
	if !ok || cluster == "" || name == "" {
		resp.Diagnostics.AddError("import id", fmt.Sprintf("%q isn't <cluster id>/<name>", req.ID))
		return
	}
	resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root("id"), req.ID)...)
	resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root("cluster"), cluster)...)
	resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root("name"), name)...)
}
