package provider

import (
	"context"
	"encoding/json"
	"net/http"
	"net/url"

	"github.com/hashicorp/terraform-plugin-framework/ephemeral"
	"github.com/hashicorp/terraform-plugin-framework/ephemeral/schema"
	"github.com/hashicorp/terraform-plugin-framework/path"
	"github.com/hashicorp/terraform-plugin-framework/types"
)

// ephemeral "roll_secrets_values": an environment's values for this run only (Terraform 1.10+),
// never in the plan or the state, as Vault's and AWS' providers offer. With dynamic = true a
// dynamic link issues a credential for the run, and closing the resource revokes its lease.
type valuesEphemeral struct{ c *client }

type valuesEphemeralModel struct {
	Project     types.String `tfsdk:"project"`
	Environment types.String `tfsdk:"environment"`
	Path        types.String `tfsdk:"path"`
	Dynamic     types.Bool   `tfsdk:"dynamic"`
	Values      types.Map    `tfsdk:"values"`
}

// leasesKey is where Open keeps what Close revokes (the project and the run's leases).
const leasesKey = "leases"

type openLeases struct {
	Project string   `json:"project"`
	Leases  []string `json:"leases"`
}

func newValuesEphemeral() ephemeral.EphemeralResource { return &valuesEphemeral{} }

func (e *valuesEphemeral) Metadata(_ context.Context, req ephemeral.MetadataRequest, resp *ephemeral.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_secrets_values"
}

func (e *valuesEphemeral) Schema(_ context.Context, _ ephemeral.SchemaRequest, resp *ephemeral.SchemaResponse) {
	resp.Schema = schema.Schema{
		Description: "An environment's values for this run only (reveal: the token must be allowed to): never written to the plan or the state.",
		Attributes: map[string]schema.Attribute{
			"project":     schema.StringAttribute{Optional: true, Computed: true},
			"environment": schema.StringAttribute{Required: true},
			"path":        schema.StringAttribute{Optional: true, Description: "The folder (default /)."},
			"dynamic":     schema.BoolAttribute{Optional: true, Description: "Issue dynamic credentials for the run (revoked when it closes). Default false: dynamic keys are left out."},
			"values":      schema.MapAttribute{Computed: true, Sensitive: true, ElementType: types.StringType},
		},
	}
}

func (e *valuesEphemeral) Configure(_ context.Context, req ephemeral.ConfigureRequest, _ *ephemeral.ConfigureResponse) {
	if c, ok := req.ProviderData.(*client); ok {
		e.c = c
	}
}

func (e *valuesEphemeral) Open(ctx context.Context, req ephemeral.OpenRequest, resp *ephemeral.OpenResponse) {
	var m valuesEphemeralModel
	resp.Diagnostics.Append(req.Config.Get(ctx, &m)...)
	if resp.Diagnostics.HasError() {
		return
	}
	p, ok := project(e.c, m.Project)
	if !ok {
		resp.Diagnostics.AddAttributeError(path.Root("project"), "which project?", "set project here or on the provider (ROLL_PROJECT)")
		return
	}
	q := url.Values{"strict": {"1"}, "dynamic": {"false"}}
	if m.Path.ValueString() != "" {
		q.Set("path", m.Path.ValueString())
	}
	if m.Dynamic.ValueBool() {
		q.Set("dynamic", "true")
	}
	var v struct {
		Values map[string]string `json:"values"`
		Leases []string          `json:"leases"`
	}
	if err := e.c.call(ctx, http.MethodGet, e.c.envPath(p, m.Environment.ValueString())+"/values?"+q.Encode(), nil, &v); err != nil {
		resp.Diagnostics.AddError("reading the values", err.Error())
		return
	}
	if len(v.Leases) > 0 {
		b, _ := json.Marshal(openLeases{Project: p, Leases: v.Leases})
		resp.Diagnostics.Append(resp.Private.SetKey(ctx, leasesKey, b)...)
	}
	values, diags := types.MapValueFrom(ctx, types.StringType, v.Values)
	resp.Diagnostics.Append(diags...)
	m.Project, m.Values = types.StringValue(p), values
	resp.Diagnostics.Append(resp.Result.Set(ctx, m)...)
}

// Close revokes the dynamic credentials the run was given.
func (e *valuesEphemeral) Close(ctx context.Context, req ephemeral.CloseRequest, resp *ephemeral.CloseResponse) {
	b, diags := req.Private.GetKey(ctx, leasesKey)
	resp.Diagnostics.Append(diags...)
	var held openLeases
	if len(b) == 0 || json.Unmarshal(b, &held) != nil {
		return
	}
	for _, l := range held.Leases {
		if err := e.c.call(ctx, http.MethodDelete, "/v1/secrets/projects/"+url.PathEscape(held.Project)+"/leases/"+url.PathEscape(l), nil, nil); err != nil && !notFound(err) {
			resp.Diagnostics.AddWarning("revoking a dynamic credential", "lease "+l+": "+err.Error()+" (it ends with its TTL)")
		}
	}
}
