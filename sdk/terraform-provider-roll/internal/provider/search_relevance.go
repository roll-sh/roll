package provider

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"net/http"
	"net/url"
	"strings"

	"github.com/hashicorp/terraform-plugin-framework/diag"
	"github.com/hashicorp/terraform-plugin-framework/path"
	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/types"
)

// ── Relevance as code: roll_search_synonyms_set and roll_search_query_ruleset ──
//
// Both are cluster-wide objects served by /v1/search (`…/synonyms/{set}`, `…/query_rules/{ruleset}`:
// Elasticsearch's `_synonyms` and `_query_rules`, run as the cluster's owner). Refreshing never
// wakes a sleeping cluster: its state stands until the cluster is loaded, as the snapshot
// repository's does.

// relevancePath is `/v1/search/clusters/{cluster}/{api}/{name}`.
func relevancePath(cluster, api, name string) string {
	return searchCluster(cluster) + "/" + api + "/" + url.PathEscape(name)
}

// loadedCluster reads the cluster: gone (remove the resource), asleep (keep the state), or loaded.
func (c *client) loadedCluster(ctx context.Context, cluster string, d *diag.Diagnostics) (gone, loaded bool) {
	var cl searchClusterAnswer
	if err := c.call(ctx, http.MethodGet, searchCluster(cluster), nil, &cl); notFound(err) {
		return true, false
	} else if err != nil {
		d.AddError("reading the cluster", err.Error())
		return false, false
	}
	return false, cl.Status.State == "loaded"
}

// importRelevance takes `<cluster id>/<name>`.
func importRelevance(ctx context.Context, req resource.ImportStateRequest, resp *resource.ImportStateResponse) {
	cluster, name, ok := strings.Cut(req.ID, "/")
	if !ok || cluster == "" || name == "" {
		resp.Diagnostics.AddError("import id", fmt.Sprintf("%q isn't <cluster id>/<name>", req.ID))
		return
	}
	resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root("id"), req.ID)...)
	resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root("cluster"), cluster)...)
	resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root("name"), name)...)
}

func relevanceIdentity(what string) map[string]schema.Attribute {
	replace := []planmodifier.String{stringplanmodifier.RequiresReplace()}
	return map[string]schema.Attribute{
		"id":      schema.StringAttribute{Computed: true, PlanModifiers: []planmodifier.String{stringplanmodifier.UseStateForUnknown()}, Description: "<cluster id>/<name>"},
		"cluster": schema.StringAttribute{Required: true, PlanModifiers: replace, Description: "The cluster's id (srch_…)."},
		"name":    schema.StringAttribute{Required: true, PlanModifiers: replace, Description: "The " + what + "'s id."},
	}
}

func deleteRelevance(ctx context.Context, c *client, p, what string, d *diag.Diagnostics) {
	if err := c.call(ctx, http.MethodDelete, p, nil, nil); err != nil && !notFound(err) {
		d.AddError("deleting the "+what, err.Error())
	}
}

// ── roll_search_synonyms_set ──

type searchSynonymsResource struct{ c *client }

type synonymRule struct {
	ID       types.String `tfsdk:"id"`
	Synonyms types.String `tfsdk:"synonyms"`
}

type searchSynonymsModel struct {
	ID      types.String  `tfsdk:"id"`
	Cluster types.String  `tfsdk:"cluster"`
	Name    types.String  `tfsdk:"name"`
	Rules   []synonymRule `tfsdk:"rules"`
}

func newSearchSynonymsResource() resource.Resource { return &searchSynonymsResource{} }

func (r *searchSynonymsResource) Metadata(_ context.Context, req resource.MetadataRequest, resp *resource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_search_synonyms_set"
}

func (r *searchSynonymsResource) Schema(_ context.Context, _ resource.SchemaRequest, resp *resource.SchemaResponse) {
	attrs := relevanceIdentity("synonyms set")
	attrs["rules"] = schema.ListNestedAttribute{
		Required:    true,
		Description: "The set's rules, in order (Elasticsearch's `synonyms_set`). Indices whose analyzers use the set reload them when it changes.",
		NestedObject: schema.NestedAttributeObject{Attributes: map[string]schema.Attribute{
			"id":       schema.StringAttribute{Required: true, Description: "The rule's id, unique in the set."},
			"synonyms": schema.StringAttribute{Required: true, Description: "Solr format: `tv, television` or `ipod => ipod, i-pod`."},
		}},
	}
	resp.Schema = schema.Schema{
		Description: "A synonyms set of a roll search cluster (`PUT _synonyms/<name>`): an index's `synonym_graph` or `synonym` filter names it with `synonyms_set`. Destroying it deletes the set; a set an index still uses isn't deleted (the error says which).",
		Attributes:  attrs,
	}
}

func (r *searchSynonymsResource) Configure(_ context.Context, req resource.ConfigureRequest, _ *resource.ConfigureResponse) {
	if c, ok := req.ProviderData.(*client); ok {
		r.c = c
	}
}

func (r *searchSynonymsResource) ValidateConfig(ctx context.Context, req resource.ValidateConfigRequest, resp *resource.ValidateConfigResponse) {
	var m searchSynonymsModel
	resp.Diagnostics.Append(req.Config.Get(ctx, &m)...)
	seen := map[string]bool{}
	for i, rule := range m.Rules {
		if rule.ID.IsUnknown() {
			continue
		}
		id := rule.ID.ValueString()
		if seen[id] {
			resp.Diagnostics.AddAttributeError(path.Root("rules").AtListIndex(i).AtName("id"), "rules", fmt.Sprintf("rule id %q is given twice", id))
		}
		seen[id] = true
	}
}

func (r *searchSynonymsResource) put(ctx context.Context, m *searchSynonymsModel, d *diag.Diagnostics) {
	set := make([]map[string]string, 0, len(m.Rules))
	for _, rule := range m.Rules {
		set = append(set, map[string]string{"id": rule.ID.ValueString(), "synonyms": rule.Synonyms.ValueString()})
	}
	if err := r.c.call(ctx, http.MethodPut, relevancePath(m.Cluster.ValueString(), "synonyms", m.Name.ValueString()), map[string]any{"synonyms_set": set}, nil); err != nil {
		d.AddError("writing the synonyms set", err.Error())
		return
	}
	m.ID = types.StringValue(m.Cluster.ValueString() + "/" + m.Name.ValueString())
}

func (r *searchSynonymsResource) Create(ctx context.Context, req resource.CreateRequest, resp *resource.CreateResponse) {
	var m searchSynonymsModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &m)...)
	if resp.Diagnostics.HasError() {
		return
	}
	if r.put(ctx, &m, &resp.Diagnostics); resp.Diagnostics.HasError() {
		return
	}
	resp.Diagnostics.Append(resp.State.Set(ctx, m)...)
}

// The most rules one read asks for: Elasticsearch's limit on a set (and `GET`'s default size is
// 10, so a read without it would see only the first ten).
const maxSynonymRules = 10000

func (r *searchSynonymsResource) Read(ctx context.Context, req resource.ReadRequest, resp *resource.ReadResponse) {
	var m searchSynonymsModel
	resp.Diagnostics.Append(req.State.Get(ctx, &m)...)
	if resp.Diagnostics.HasError() {
		return
	}
	gone, loaded := r.c.loadedCluster(ctx, m.Cluster.ValueString(), &resp.Diagnostics)
	if gone {
		resp.State.RemoveResource(ctx)
		return
	}
	if resp.Diagnostics.HasError() || (!loaded && m.Rules != nil) {
		return
	}
	var got struct {
		SynonymsSet []struct {
			ID       string `json:"id"`
			Synonyms string `json:"synonyms"`
		} `json:"synonyms_set"`
	}
	p := relevancePath(m.Cluster.ValueString(), "synonyms", m.Name.ValueString()) + fmt.Sprintf("?size=%d", maxSynonymRules)
	if err := r.c.call(ctx, http.MethodGet, p, nil, &got); notFound(err) {
		resp.State.RemoveResource(ctx)
		return
	} else if err != nil {
		resp.Diagnostics.AddError("reading the synonyms set", err.Error())
		return
	}
	m.Rules = make([]synonymRule, 0, len(got.SynonymsSet))
	for _, s := range got.SynonymsSet {
		m.Rules = append(m.Rules, synonymRule{ID: types.StringValue(s.ID), Synonyms: types.StringValue(s.Synonyms)})
	}
	resp.Diagnostics.Append(resp.State.Set(ctx, m)...)
}

func (r *searchSynonymsResource) Update(ctx context.Context, req resource.UpdateRequest, resp *resource.UpdateResponse) {
	var m searchSynonymsModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &m)...)
	if resp.Diagnostics.HasError() {
		return
	}
	if r.put(ctx, &m, &resp.Diagnostics); resp.Diagnostics.HasError() {
		return
	}
	resp.Diagnostics.Append(resp.State.Set(ctx, m)...)
}

func (r *searchSynonymsResource) Delete(ctx context.Context, req resource.DeleteRequest, resp *resource.DeleteResponse) {
	var m searchSynonymsModel
	resp.Diagnostics.Append(req.State.Get(ctx, &m)...)
	deleteRelevance(ctx, r.c, relevancePath(m.Cluster.ValueString(), "synonyms", m.Name.ValueString()), "synonyms set", &resp.Diagnostics)
}

func (r *searchSynonymsResource) ImportState(ctx context.Context, req resource.ImportStateRequest, resp *resource.ImportStateResponse) {
	importRelevance(ctx, req, resp)
}

// ── roll_search_query_ruleset ──

type searchRulesetResource struct{ c *client }

type searchRulesetModel struct {
	ID      types.String `tfsdk:"id"`
	Cluster types.String `tfsdk:"cluster"`
	Name    types.String `tfsdk:"name"`
	Rules   types.String `tfsdk:"rules"`
}

func newSearchRulesetResource() resource.Resource { return &searchRulesetResource{} }

func (r *searchRulesetResource) Metadata(_ context.Context, req resource.MetadataRequest, resp *resource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_search_query_ruleset"
}

func (r *searchRulesetResource) Schema(_ context.Context, _ resource.SchemaRequest, resp *resource.SchemaResponse) {
	attrs := relevanceIdentity("query ruleset")
	attrs["rules"] = schema.StringAttribute{
		Required: true,
		Description: "The ruleset's rules as JSON (`jsonencode([...])`), as Elasticsearch takes them: " +
			"`[{rule_id, type = \"pinned\" | \"exclude\", criteria = [{type, metadata, values}], actions = {ids | docs}, priority?}]`. " +
			"Compared by meaning, so key order and spacing make no change.",
	}
	resp.Schema = schema.Schema{
		Description: "A query ruleset of a roll search cluster (`PUT _query_rules/<name>`): pins or excludes documents for queries that match its criteria, through a `rule` query or retriever naming it. Destroying it deletes the ruleset.",
		Attributes:  attrs,
	}
}

func (r *searchRulesetResource) Configure(_ context.Context, req resource.ConfigureRequest, _ *resource.ConfigureResponse) {
	if c, ok := req.ProviderData.(*client); ok {
		r.c = c
	}
}

func (r *searchRulesetResource) ValidateConfig(ctx context.Context, req resource.ValidateConfigRequest, resp *resource.ValidateConfigResponse) {
	var m searchRulesetModel
	resp.Diagnostics.Append(req.Config.Get(ctx, &m)...)
	if !given(m.Rules) {
		return
	}
	var rules []any
	if err := json.Unmarshal([]byte(m.Rules.ValueString()), &rules); err != nil {
		resp.Diagnostics.AddAttributeError(path.Root("rules"), "rules", "a JSON array of rules (jsonencode([...])): "+err.Error())
	}
}

// canonicalJSON is `raw` re-encoded with sorted keys and no spacing: two encodings of the same
// value compare equal.
func canonicalJSON(raw []byte) (string, error) {
	var v any
	dec := json.NewDecoder(bytes.NewReader(raw))
	dec.UseNumber()
	if err := dec.Decode(&v); err != nil {
		return "", err
	}
	out, err := json.Marshal(v)
	return string(out), err
}

func (r *searchRulesetResource) put(ctx context.Context, m *searchRulesetModel, d *diag.Diagnostics) {
	var rules json.RawMessage = []byte(m.Rules.ValueString())
	if err := r.c.call(ctx, http.MethodPut, relevancePath(m.Cluster.ValueString(), "query_rules", m.Name.ValueString()), map[string]any{"rules": rules}, nil); err != nil {
		d.AddError("writing the query ruleset", err.Error())
		return
	}
	m.ID = types.StringValue(m.Cluster.ValueString() + "/" + m.Name.ValueString())
}

func (r *searchRulesetResource) Create(ctx context.Context, req resource.CreateRequest, resp *resource.CreateResponse) {
	var m searchRulesetModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &m)...)
	if resp.Diagnostics.HasError() {
		return
	}
	if r.put(ctx, &m, &resp.Diagnostics); resp.Diagnostics.HasError() {
		return
	}
	resp.Diagnostics.Append(resp.State.Set(ctx, m)...)
}

// Read keeps the configuration's own text while the ruleset means the same (Elasticsearch
// answers with its own key order), and takes the cluster's when it changed elsewhere.
func (r *searchRulesetResource) Read(ctx context.Context, req resource.ReadRequest, resp *resource.ReadResponse) {
	var m searchRulesetModel
	resp.Diagnostics.Append(req.State.Get(ctx, &m)...)
	if resp.Diagnostics.HasError() {
		return
	}
	gone, loaded := r.c.loadedCluster(ctx, m.Cluster.ValueString(), &resp.Diagnostics)
	if gone {
		resp.State.RemoveResource(ctx)
		return
	}
	if resp.Diagnostics.HasError() || (!loaded && given(m.Rules)) {
		return
	}
	var got struct {
		Rules json.RawMessage `json:"rules"`
	}
	if err := r.c.call(ctx, http.MethodGet, relevancePath(m.Cluster.ValueString(), "query_rules", m.Name.ValueString()), nil, &got); notFound(err) {
		resp.State.RemoveResource(ctx)
		return
	} else if err != nil {
		resp.Diagnostics.AddError("reading the query ruleset", err.Error())
		return
	}
	read, err := canonicalJSON(got.Rules)
	if err != nil {
		resp.Diagnostics.AddError("reading the query ruleset", "its rules aren't JSON: "+err.Error())
		return
	}
	if had, err := canonicalJSON([]byte(m.Rules.ValueString())); err != nil || had != read {
		m.Rules = types.StringValue(read)
	}
	resp.Diagnostics.Append(resp.State.Set(ctx, m)...)
}

func (r *searchRulesetResource) Update(ctx context.Context, req resource.UpdateRequest, resp *resource.UpdateResponse) {
	var m searchRulesetModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &m)...)
	if resp.Diagnostics.HasError() {
		return
	}
	if r.put(ctx, &m, &resp.Diagnostics); resp.Diagnostics.HasError() {
		return
	}
	resp.Diagnostics.Append(resp.State.Set(ctx, m)...)
}

func (r *searchRulesetResource) Delete(ctx context.Context, req resource.DeleteRequest, resp *resource.DeleteResponse) {
	var m searchRulesetModel
	resp.Diagnostics.Append(req.State.Get(ctx, &m)...)
	deleteRelevance(ctx, r.c, relevancePath(m.Cluster.ValueString(), "query_rules", m.Name.ValueString()), "query ruleset", &resp.Diagnostics)
}

func (r *searchRulesetResource) ImportState(ctx context.Context, req resource.ImportStateRequest, resp *resource.ImportStateResponse) {
	importRelevance(ctx, req, resp)
}
