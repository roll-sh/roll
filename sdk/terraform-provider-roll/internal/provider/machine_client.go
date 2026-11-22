package provider

import (
	"context"
	"encoding/json"
	"fmt"
	"net/http"
	"net/url"
	"time"

	"github.com/hashicorp/terraform-plugin-framework-validators/int64validator"
	"github.com/hashicorp/terraform-plugin-framework-validators/listvalidator"
	"github.com/hashicorp/terraform-plugin-framework-validators/mapvalidator"
	"github.com/hashicorp/terraform-plugin-framework-validators/resourcevalidator"
	"github.com/hashicorp/terraform-plugin-framework-validators/stringvalidator"
	"github.com/hashicorp/terraform-plugin-framework/attr"
	"github.com/hashicorp/terraform-plugin-framework/diag"
	"github.com/hashicorp/terraform-plugin-framework/path"
	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/schema/validator"
	"github.com/hashicorp/terraform-plugin-framework/types"
)

// roll_machine_client: automation's OAuth client credentials (docs/permissions.md, "Machine
// clients", /v1/access/clients). It proves itself with CI's own OIDC token or a public key, never a
// secret roll makes (a secret in the state is what this avoids). Rules and expiry change in place
// on its grant (PATCH /v1/access/tokens/{grant}); a new credential rotates it (the client id stays).
type machineClientResource struct{ c *client }

type machineClientModel struct {
	ID            types.String `tfsdk:"id"`
	ClientID      types.String `tfsdk:"client_id"`
	GrantID       types.String `tfsdk:"grant_id"`
	TokenEndpoint types.String `tfsdk:"token_endpoint"`
	Name          types.String `tfsdk:"name"`
	Rules         []ruleModel  `tfsdk:"rule"`
	ExpiresAt     types.String `tfsdk:"expires_at"`
	IPAllow       types.List   `tfsdk:"ip_allow"`
	RatePerMinute types.Int64  `tfsdk:"rate_per_minute"`
	PublicJWK     types.String `tfsdk:"public_jwk"`
	OIDC          *oidcModel   `tfsdk:"oidc"`
	KeepOldForS   types.Int64  `tfsdk:"keep_old_for_s"`
}

// ruleModel is one of roll_auth::scope's rules, in its JSON names.
type ruleModel struct {
	Product      types.String    `tfsdk:"product"`
	Resources    types.String    `tfsdk:"resources"`
	Resource     []selectorModel `tfsdk:"resource"`
	Environments types.List      `tfsdk:"environments"`
	Paths        types.List      `tfsdk:"paths"`
	Actions      types.Map       `tfsdk:"actions"`
}

type selectorModel struct {
	ID      types.String `tfsdk:"id"`
	Index   types.String `tfsdk:"index"`
	Project types.String `tfsdk:"project"`
}

type oidcModel struct {
	Issuer   types.String `tfsdk:"issuer"`
	Audience types.String `tfsdk:"audience"`
	Subject  types.String `tfsdk:"subject"`
	Claims   types.Map    `tfsdk:"claims"`
	JWKS     types.String `tfsdk:"jwks"`
}

func newMachineClientResource() resource.Resource { return &machineClientResource{} }

func (r *machineClientResource) Metadata(_ context.Context, req resource.MetadataRequest, resp *resource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_machine_client"
}

func (r *machineClientResource) Schema(_ context.Context, _ resource.SchemaRequest, resp *resource.SchemaResponse) {
	computed := func(desc string) schema.StringAttribute {
		return schema.StringAttribute{Computed: true, Description: desc, PlanModifiers: []planmodifier.String{stringplanmodifier.UseStateForUnknown()}}
	}
	strs := func(desc string) schema.ListAttribute {
		return schema.ListAttribute{Optional: true, ElementType: types.StringType, Description: desc}
	}
	resp.Schema = schema.Schema{
		Description: "A machine client: automation (CI, a scheduled job) trades CI's own OIDC token, or a JWT signed with its key, at roll's token endpoint for an hour's token its rules bound, so nothing long-lived sits in its settings. Exactly one of oidc and public_jwk: there's no secret mode. Machine clients are made with the operator's token (ROLL_TOKEN), not a scoped one. Destroying it revokes it and every token it got.",
		Attributes: map[string]schema.Attribute{
			"id":             computed("The client id (mcl_…)."),
			"client_id":      computed("The client id (mcl_…) the token request names."),
			"grant_id":       computed("The grant holding its rules (tok_…), as roll's token pages show it."),
			"token_endpoint": computed("Where it trades its credential for a token."),
			"name": schema.StringAttribute{Required: true, PlanModifiers: []planmodifier.String{stringplanmodifier.RequiresReplace()},
				Description: "Its name. roll has no way to rename one, so a new name replaces it (a new client id)."},
			"expires_at":      schema.StringAttribute{Optional: true, Description: "When it stops working (RFC 3339); never when left out (the token policy may require one)."},
			"ip_allow":        strs("Client addresses or CIDRs it works from (any when left out)."),
			"rate_per_minute": schema.Int64Attribute{Optional: true, Description: "Requests a minute its tokens may make, per node (no limit when left out)."},
			"public_jwk":      schema.StringAttribute{Optional: true, Description: "The public key (a JWK as JSON: jsonencode it) it signs its assertions with: Ed25519 (OKP), P-256 (EC) or RSA of 2048 bits or more, never a private part. roll doesn't answer it, so it isn't read back: after an import, the first apply registers it again (a rotate)."},
			"keep_old_for_s": schema.Int64Attribute{Optional: true, Description: "When the credential changes, how long the old one (and the tokens it got) keeps working, 0 to 604800 seconds (0 when left out: it stops at once). Only read when rotating.",
				Validators: []validator.Int64{int64validator.Between(0, 604800)}},
		},
		Blocks: map[string]schema.Block{
			"rule": schema.ListNestedBlock{
				Description: "One of its rules, as roll_auth's scope names them: what it may do on one product's resources.",
				Validators:  []validator.List{listvalidator.SizeAtLeast(1)},
				NestedObject: schema.NestedBlockObject{
					Attributes: map[string]schema.Attribute{
						"product":      schema.StringAttribute{Required: true, Description: "A product (apps, secrets, postgres…), or * for every one."},
						"resources":    schema.StringAttribute{Optional: true, Description: `"*": every resource of the product. Otherwise name them in resource blocks (exactly one of the two).`, Validators: []validator.String{stringvalidator.OneOf("*")}},
						"environments": strs("Only resources whose environment is one of these (pv-* covers pv-42); all when left out."),
						"paths":        strs("Only resources under one of these paths (/api covers /api/x); all when left out."),
						"actions": schema.MapAttribute{Required: true, ElementType: types.StringType, Description: `Each action (read, write, deploy…) and "allow", or "ask" to have a person approve each use.`,
							Validators: []validator.Map{mapvalidator.SizeAtLeast(1), mapvalidator.ValueStringsAre(stringvalidator.OneOf("allow", "ask"))}},
					},
					Blocks: map[string]schema.Block{
						"resource": schema.ListNestedBlock{
							Description: "One resource the rule covers: id (and optionally index), or project.",
							NestedObject: schema.NestedBlockObject{Attributes: map[string]schema.Attribute{
								"id":      schema.StringAttribute{Optional: true, Description: "The product's own id (svc_…, ep_…)."},
								"index":   schema.StringAttribute{Optional: true, Description: "With id: only these named parts of it (search's indices, logs-*,a)."},
								"project": schema.StringAttribute{Optional: true, Description: "Every resource of the product on this project (prj_…)."},
							}},
						},
					},
				},
			},
			"oidc": schema.SingleNestedBlock{
				Description: "The identity it trusts: CI's own OIDC token signs it in (trusted publishing), so nothing is stored anywhere.",
				Attributes: map[string]schema.Attribute{
					"issuer": schema.StringAttribute{Required: true, Description: "The OIDC token's iss, e.g. https://token.actions.githubusercontent.com."},
					"audience": schema.StringAttribute{Optional: true, Computed: true, PlanModifiers: []planmodifier.String{stringplanmodifier.UseStateForUnknown()},
						Description: "The token's aud: the API's URL, as the CLI and the GitHub Action ask for (roll's API URL when left out)."},
					"subject": schema.StringAttribute{Required: true, Description: "The token's sub, or a pattern of it with * (repo:acme/web:ref:refs/heads/main). Shared issuers must name one repository."},
					"claims":  schema.MapAttribute{Optional: true, ElementType: types.StringType, Description: "Other claims that must match: a name (ref, environment…) or a JSON pointer (/realm_access/roles) to a value or a pattern with *. An array claim matches when any element does."},
					"jwks":    schema.StringAttribute{Optional: true, Description: "The issuer's keys pinned (a JWKS document as JSON), for an issuer roll can't reach. roll answers only how many keys are pinned, so it isn't read back."},
				},
			},
		},
	}
}

func (r *machineClientResource) ConfigValidators(context.Context) []resource.ConfigValidator {
	return []resource.ConfigValidator{resourcevalidator.ExactlyOneOf(path.MatchRoot("oidc"), path.MatchRoot("public_jwk"))}
}

// ValidateConfig checks what the schema can't say: what a rule's resources name.
func (r *machineClientResource) ValidateConfig(ctx context.Context, req resource.ValidateConfigRequest, resp *resource.ValidateConfigResponse) {
	var m machineClientModel
	if req.Config.Get(ctx, &m).HasError() {
		return // not known yet: checked again when it is
	}
	for i, rule := range m.Rules {
		_, err := ruleJSONOf(ctx, rule, &diag.Diagnostics{})
		if err != "" {
			resp.Diagnostics.AddAttributeError(path.Root("rule").AtListIndex(i), "rule", err)
		}
	}
}

func (r *machineClientResource) Configure(_ context.Context, req resource.ConfigureRequest, _ *resource.ConfigureResponse) {
	if c, ok := req.ProviderData.(*client); ok {
		r.c = c
	}
}

func (r *machineClientResource) ImportState(ctx context.Context, req resource.ImportStateRequest, resp *resource.ImportStateResponse) {
	resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root("id"), req.ID)...)
}

// The wire: roll_auth::scope's rule, and the grant and listed client roll answers.
type ruleJSON struct {
	Product      string            `json:"product"`
	Resources    any               `json:"resources"`
	Environments *[]string         `json:"environments,omitempty"`
	Paths        *[]string         `json:"paths,omitempty"`
	Actions      map[string]string `json:"actions"`
}

type selectorJSON struct {
	ID      string `json:"id,omitempty"`
	Index   string `json:"index,omitempty"`
	Project string `json:"project,omitempty"`
}

type grantJSON struct {
	ID            string            `json:"id"`
	Name          string            `json:"name"`
	Rules         []json.RawMessage `json:"rules"`
	ExpiresAt     *int64            `json:"expires_at"`
	IPAllow       []string          `json:"ip_allow"`
	RatePerMinute *int64            `json:"rate_per_minute"`
	RevokedAt     *int64            `json:"revoked_at"`
}

type listedClientJSON struct {
	ClientID     string `json:"client_id"`
	SignsWithKey bool   `json:"signs_with_key"`
	Trusts       *struct {
		Issuer   string            `json:"issuer"`
		Audience string            `json:"audience"`
		Subject  string            `json:"subject"`
		Claims   map[string]string `json:"claims"`
	} `json:"trusts"`
	Grant grantJSON `json:"grant"`
}

// optStrings is a list's strings, nil only when it's unset: an empty list stays empty (it covers
// nothing), never dropped into "all".
func optStrings(ctx context.Context, l types.List, d *diag.Diagnostics) *[]string {
	if l.IsNull() || l.IsUnknown() {
		return nil
	}
	out := []string{}
	d.Append(l.ElementsAs(ctx, &out, false)...)
	return &out
}

func named(s string) bool { return s != "" }

// ruleJSONOf is a rule as roll takes it, or why it isn't one.
func ruleJSONOf(ctx context.Context, m ruleModel, d *diag.Diagnostics) (ruleJSON, string) {
	out := ruleJSON{Product: m.Product.ValueString(), Environments: optStrings(ctx, m.Environments, d), Paths: optStrings(ctx, m.Paths, d), Actions: map[string]string{}}
	if !m.Actions.IsNull() && !m.Actions.IsUnknown() {
		d.Append(m.Actions.ElementsAs(ctx, &out.Actions, false)...)
	}
	all := !m.Resources.IsNull()
	if all == (len(m.Resource) > 0) {
		return out, `exactly one of resources = "*" and resource blocks naming them`
	}
	if all {
		out.Resources = "*"
		return out, ""
	}
	sels := []selectorJSON{}
	for _, s := range m.Resource {
		sel := selectorJSON{ID: s.ID.ValueString(), Index: s.Index.ValueString(), Project: s.Project.ValueString()}
		known := !s.ID.IsUnknown() && !s.Project.IsUnknown()
		if known && named(sel.ID) == named(sel.Project) {
			return out, "a resource block names exactly one of id and project"
		}
		if known && named(sel.Index) && !named(sel.ID) {
			return out, "index narrows a resource named by id"
		}
		sels = append(sels, sel)
	}
	out.Resources = sels
	return out, ""
}

func rulesJSON(ctx context.Context, rules []ruleModel, d *diag.Diagnostics) []ruleJSON {
	out := make([]ruleJSON, 0, len(rules))
	for i, m := range rules {
		r, err := ruleJSONOf(ctx, m, d)
		if err != "" {
			d.AddAttributeError(path.Root("rule").AtListIndex(i), "rule", err)
		}
		out = append(out, r)
	}
	return out
}

// rulesModel is roll's rules as the blocks (the state's, `was`, when roll's don't parse).
func rulesModel(ctx context.Context, raw []json.RawMessage, was []ruleModel, d *diag.Diagnostics) []ruleModel {
	out := make([]ruleModel, 0, len(raw))
	for _, b := range raw {
		var r struct {
			Product      string            `json:"product"`
			Resources    json.RawMessage   `json:"resources"`
			Environments *[]string         `json:"environments"`
			Paths        *[]string         `json:"paths"`
			Actions      map[string]string `json:"actions"`
		}
		if err := json.Unmarshal(b, &r); err != nil {
			d.AddError("reading the machine client's rules", err.Error())
			return was
		}
		m := ruleModel{Product: types.StringValue(r.Product), Resources: types.StringNull(), Resource: []selectorModel{}, Environments: optList(r.Environments), Paths: optList(r.Paths)}
		var sels []selectorJSON
		if string(r.Resources) == `"*"` {
			m.Resources = types.StringValue("*")
		} else if err := json.Unmarshal(r.Resources, &sels); err != nil {
			d.AddError("reading the machine client's rules", err.Error())
			return was
		}
		for _, s := range sels {
			m.Resource = append(m.Resource, selectorModel{ID: stringOrNull(s.ID), Index: stringOrNull(s.Index), Project: stringOrNull(s.Project)})
		}
		m.Actions = mapOfStrings(ctx, r.Actions, d)
		out = append(out, m)
	}
	return out
}

func optList(xs *[]string) types.List {
	if xs == nil {
		return types.ListNull(types.StringType)
	}
	vs := make([]attr.Value, len(*xs))
	for i, x := range *xs {
		vs[i] = types.StringValue(x)
	}
	return types.ListValueMust(types.StringType, vs)
}

func mapOfStrings(ctx context.Context, m map[string]string, d *diag.Diagnostics) types.Map {
	if m == nil {
		m = map[string]string{}
	}
	v, diags := types.MapValueFrom(ctx, types.StringType, m)
	d.Append(diags...)
	return v
}

func stringOrNull(s string) types.String {
	if s == "" {
		return types.StringNull()
	}
	return types.StringValue(s)
}

// expiresMs is expires_at as roll takes it (nil: never).
func expiresMs(v types.String, d *diag.Diagnostics) *int64 {
	if v.IsNull() || v.IsUnknown() {
		return nil
	}
	at, err := time.Parse(time.RFC3339, v.ValueString())
	if err != nil {
		d.AddAttributeError(path.Root("expires_at"), "expires_at", "an RFC 3339 time, e.g. 2027-01-01T00:00:00Z")
		return nil
	}
	ms := at.UnixMilli()
	return &ms
}

func int64OrNil(v types.Int64) *int64 {
	if v.IsNull() || v.IsUnknown() {
		return nil
	}
	n := v.ValueInt64()
	return &n
}

// credential is the body's public_jwk or oidc.
func credential(ctx context.Context, m *machineClientModel, body map[string]any, d *diag.Diagnostics) {
	if !m.PublicJWK.IsNull() && !m.PublicJWK.IsUnknown() {
		var jwk map[string]any
		if err := json.Unmarshal([]byte(m.PublicJWK.ValueString()), &jwk); err != nil {
			d.AddAttributeError(path.Root("public_jwk"), "public_jwk", "a JWK as JSON, e.g. jsonencode({ kty = \"OKP\", crv = \"Ed25519\", x = \"…\" })")
			return
		}
		body["public_jwk"] = jwk
	}
	if o := m.OIDC; o != nil {
		oidc := map[string]any{"issuer": o.Issuer.ValueString(), "subject": o.Subject.ValueString()}
		if !o.Audience.IsNull() && !o.Audience.IsUnknown() {
			oidc["audience"] = o.Audience.ValueString()
		}
		if !o.Claims.IsNull() && !o.Claims.IsUnknown() {
			claims := map[string]string{}
			d.Append(o.Claims.ElementsAs(ctx, &claims, false)...)
			oidc["claims"] = claims
		}
		if !o.JWKS.IsNull() && !o.JWKS.IsUnknown() {
			oidc["jwks"] = o.JWKS.ValueString()
		}
		body["oidc"] = oidc
	}
}

// failed reports a call that didn't work, naming the token a 403 means.
func failed(d *diag.Diagnostics, what string, err error) {
	detail := err.Error()
	if e, ok := err.(*apiError); ok && e.status == http.StatusForbidden {
		detail += ": machine clients are made with the operator's token (ROLL_TOKEN), not a scoped one"
	}
	d.AddError(what, detail)
}

func (r *machineClientResource) path(id string) string {
	return "/v1/access/clients/" + url.PathEscape(id)
}

// find is the client with this id from roll's list (nil: there's none).
func (r *machineClientResource) find(ctx context.Context, id string) (*listedClientJSON, error) {
	var all []listedClientJSON
	if err := r.c.call(ctx, http.MethodGet, "/v1/access/clients", nil, &all); err != nil {
		return nil, err
	}
	for i := range all {
		if all[i].ClientID == id {
			return &all[i], nil
		}
	}
	return nil, nil
}

// audience fills an audience roll chose (left out of the file) from what it lists.
func (r *machineClientResource) audience(ctx context.Context, m *machineClientModel, d *diag.Diagnostics) {
	if m.OIDC == nil || !m.OIDC.Audience.IsUnknown() {
		return
	}
	c, err := r.find(ctx, m.ClientID.ValueString())
	if err != nil || c == nil || c.Trusts == nil {
		// Never unknown in the state: the next refresh reads it.
		m.OIDC.Audience = types.StringNull()
		d.AddError("reading the machine client's audience", fmt.Sprintf("it's made (%s), but listing it failed: %v", m.ClientID.ValueString(), err))
		return
	}
	m.OIDC.Audience = types.StringValue(c.Trusts.Audience)
}

func (r *machineClientResource) Create(ctx context.Context, req resource.CreateRequest, resp *resource.CreateResponse) {
	var m machineClientModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &m)...)
	if resp.Diagnostics.HasError() {
		return
	}
	body := map[string]any{"name": m.Name.ValueString(), "rules": rulesJSON(ctx, m.Rules, &resp.Diagnostics)}
	if at := expiresMs(m.ExpiresAt, &resp.Diagnostics); at != nil {
		body["expires_at"] = *at
	}
	if ips := optStrings(ctx, m.IPAllow, &resp.Diagnostics); ips != nil {
		body["ip_allow"] = *ips
	}
	if n := int64OrNil(m.RatePerMinute); n != nil {
		body["rate_per_minute"] = *n
	}
	credential(ctx, &m, body, &resp.Diagnostics)
	if resp.Diagnostics.HasError() {
		return
	}
	var made struct {
		ClientID      string    `json:"client_id"`
		Grant         grantJSON `json:"grant"`
		TokenEndpoint string    `json:"token_endpoint"`
	}
	if err := r.c.call(ctx, http.MethodPost, "/v1/access/clients", body, &made); err != nil {
		failed(&resp.Diagnostics, "creating the machine client", err)
		return
	}
	m.ID, m.ClientID, m.GrantID, m.TokenEndpoint = types.StringValue(made.ClientID), types.StringValue(made.ClientID), types.StringValue(made.Grant.ID), types.StringValue(made.TokenEndpoint)
	// Saved even when this fails, so a made client is never lost from the state.
	r.audience(ctx, &m, &resp.Diagnostics)
	resp.Diagnostics.Append(resp.State.Set(ctx, m)...)
}

func (r *machineClientResource) Read(ctx context.Context, req resource.ReadRequest, resp *resource.ReadResponse) {
	var m machineClientModel
	resp.Diagnostics.Append(req.State.Get(ctx, &m)...)
	if resp.Diagnostics.HasError() {
		return
	}
	c, err := r.find(ctx, m.ID.ValueString())
	if err != nil && !notFound(err) {
		failed(&resp.Diagnostics, "reading the machine client", err)
		return
	}
	if c == nil || c.Grant.RevokedAt != nil || (c.Grant.ExpiresAt != nil && *c.Grant.ExpiresAt <= time.Now().UnixMilli()) {
		resp.State.RemoveResource(ctx)
		return
	}
	g := &c.Grant
	m.ClientID, m.GrantID, m.Name = types.StringValue(c.ClientID), types.StringValue(g.ID), types.StringValue(g.Name)
	m.Rules = rulesModel(ctx, g.Rules, m.Rules, &resp.Diagnostics)
	if was := expiresMs(m.ExpiresAt, &diag.Diagnostics{}); g.ExpiresAt == nil {
		m.ExpiresAt = types.StringNull()
	} else if was == nil || *was != *g.ExpiresAt {
		m.ExpiresAt = types.StringValue(time.UnixMilli(*g.ExpiresAt).UTC().Format(time.RFC3339))
	}
	m.IPAllow = listOf(g.IPAllow, m.IPAllow)
	m.RatePerMinute = types.Int64PointerValue(g.RatePerMinute)
	if t := c.Trusts; t == nil {
		m.OIDC = nil
		if !c.SignsWithKey {
			// Moved to a secret outside Terraform: the next apply registers the key again.
			m.PublicJWK = types.StringNull()
		}
	} else {
		o := oidcModel{Issuer: types.StringValue(t.Issuer), Audience: types.StringValue(t.Audience), Subject: types.StringValue(t.Subject), Claims: types.MapNull(types.StringType), JWKS: types.StringNull()}
		if m.OIDC != nil {
			o.JWKS = m.OIDC.JWKS // only a count of its keys is answered
			o.Claims = m.OIDC.Claims
		}
		if len(t.Claims) > 0 || !o.Claims.IsNull() {
			o.Claims = mapOfStrings(ctx, t.Claims, &resp.Diagnostics)
		}
		m.OIDC, m.PublicJWK = &o, types.StringNull()
	}
	if m.TokenEndpoint.IsNull() {
		// Imported: roll's metadata names it, as the create answer did.
		var meta struct {
			TokenEndpoint string `json:"token_endpoint"`
		}
		if err := r.c.call(ctx, http.MethodGet, "/.well-known/oauth-authorization-server", nil, &meta); err != nil {
			failed(&resp.Diagnostics, "reading roll's token endpoint", err)
			return
		}
		m.TokenEndpoint = types.StringValue(meta.TokenEndpoint)
	}
	resp.Diagnostics.Append(resp.State.Set(ctx, m)...)
}

func (r *machineClientResource) Update(ctx context.Context, req resource.UpdateRequest, resp *resource.UpdateResponse) {
	var plan, state machineClientModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	if resp.Diagnostics.HasError() {
		return
	}
	plan.ID, plan.ClientID, plan.GrantID, plan.TokenEndpoint = state.ID, state.ClientID, state.GrantID, state.TokenEndpoint

	// Rules, expiry and limits are the grant's: changed in place.
	change := map[string]any{}
	if rules := rulesJSON(ctx, plan.Rules, &resp.Diagnostics); !jsonEqual(rules, rulesJSON(ctx, state.Rules, &diag.Diagnostics{})) {
		change["rules"] = rules
	}
	if !plan.ExpiresAt.Equal(state.ExpiresAt) {
		change["expires_at"] = expiresMs(plan.ExpiresAt, &resp.Diagnostics)
	}
	if !plan.IPAllow.Equal(state.IPAllow) {
		ips := optStrings(ctx, plan.IPAllow, &resp.Diagnostics)
		if ips == nil {
			ips = &[]string{}
		}
		change["ip_allow"] = *ips
	}
	if !plan.RatePerMinute.Equal(state.RatePerMinute) {
		change["rate_per_minute"] = int64OrNil(plan.RatePerMinute)
	}
	// A new credential rotates it: the client id and the rules stay.
	rotate := map[string]any{}
	if !plan.PublicJWK.Equal(state.PublicJWK) || !oidcEqual(plan.OIDC, state.OIDC) {
		credential(ctx, &plan, rotate, &resp.Diagnostics)
		if n := int64OrNil(plan.KeepOldForS); n != nil {
			rotate["keep_old_for_s"] = *n
		}
	}
	if resp.Diagnostics.HasError() {
		return
	}
	if len(change) > 0 {
		if err := r.c.call(ctx, http.MethodPatch, "/v1/access/tokens/"+url.PathEscape(state.GrantID.ValueString()), change, nil); err != nil {
			failed(&resp.Diagnostics, "changing the machine client's rules", err)
			return
		}
	}
	if len(rotate) > 0 {
		if err := r.c.call(ctx, http.MethodPost, r.path(state.ClientID.ValueString())+"/rotate", rotate, nil); err != nil {
			failed(&resp.Diagnostics, "rotating the machine client's credential", err)
			// The rules changed above stay changed: say so in the state.
			plan.PublicJWK, plan.OIDC = state.PublicJWK, state.OIDC
			resp.Diagnostics.Append(resp.State.Set(ctx, plan)...)
			return
		}
	}
	r.audience(ctx, &plan, &resp.Diagnostics)
	resp.Diagnostics.Append(resp.State.Set(ctx, plan)...)
}

func jsonEqual(a, b any) bool {
	x, _ := json.Marshal(a)
	y, _ := json.Marshal(b)
	return string(x) == string(y)
}

func oidcEqual(a, b *oidcModel) bool {
	if a == nil || b == nil {
		return a == b
	}
	return a.Issuer.Equal(b.Issuer) && a.Audience.Equal(b.Audience) && a.Subject.Equal(b.Subject) && a.Claims.Equal(b.Claims) && a.JWKS.Equal(b.JWKS)
}

func (r *machineClientResource) Delete(ctx context.Context, req resource.DeleteRequest, resp *resource.DeleteResponse) {
	var m machineClientModel
	resp.Diagnostics.Append(req.State.Get(ctx, &m)...)
	if resp.Diagnostics.HasError() {
		return
	}
	if err := r.c.call(ctx, http.MethodDelete, r.path(m.ClientID.ValueString()), nil, nil); err != nil && !notFound(err) {
		failed(&resp.Diagnostics, "revoking the machine client", err)
	}
}
