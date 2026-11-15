package provider

import (
	"context"
	"net/http"

	"github.com/hashicorp/terraform-plugin-framework/datasource"
	"github.com/hashicorp/terraform-plugin-framework/datasource/schema"
	"github.com/hashicorp/terraform-plugin-framework/path"
	"github.com/hashicorp/terraform-plugin-framework/types"
)

// roll_secrets_values: an environment's values (references resolved), for another provider's
// configuration. They land in Terraform's state: keep that state as private as the values.
type valuesDataSource struct{ c *client }

type valuesModel struct {
	Project     types.String `tfsdk:"project"`
	Environment types.String `tfsdk:"environment"`
	Path        types.String `tfsdk:"path"`
	Values      types.Map    `tfsdk:"values"`
}

func newValuesDataSource() datasource.DataSource { return &valuesDataSource{} }

func (d *valuesDataSource) Metadata(_ context.Context, req datasource.MetadataRequest, resp *datasource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_secrets_values"
}

func (d *valuesDataSource) Schema(_ context.Context, _ datasource.SchemaRequest, resp *datasource.SchemaResponse) {
	resp.Schema = schema.Schema{
		Description: "An environment's values, references resolved (reveal: the token must be allowed to). They are written to Terraform's state.",
		Attributes: map[string]schema.Attribute{
			"project":     schema.StringAttribute{Optional: true, Computed: true},
			"environment": schema.StringAttribute{Required: true},
			"path":        schema.StringAttribute{Optional: true, Description: "The folder (default /)."},
			"values":      schema.MapAttribute{Computed: true, Sensitive: true, ElementType: types.StringType},
		},
	}
}

func (d *valuesDataSource) Configure(_ context.Context, req datasource.ConfigureRequest, _ *datasource.ConfigureResponse) {
	if c, ok := req.ProviderData.(*client); ok {
		d.c = c
	}
}

func (d *valuesDataSource) Read(ctx context.Context, req datasource.ReadRequest, resp *datasource.ReadResponse) {
	var m valuesModel
	resp.Diagnostics.Append(req.Config.Get(ctx, &m)...)
	if resp.Diagnostics.HasError() {
		return
	}
	p, ok := project(d.c, m.Project)
	if !ok {
		resp.Diagnostics.AddAttributeError(path.Root("project"), "which project?", "set project here or on the provider (ROLL_PROJECT)")
		return
	}
	var v struct {
		Values   map[string]string `json:"values"`
		Problems []struct {
			Key     string `json:"key"`
			Problem string `json:"problem"`
		} `json:"problems"`
	}
	if err := d.c.call(ctx, http.MethodGet, d.c.envPath(p, m.Environment.ValueString())+"/values"+pathQuery(m.Path.ValueString()), nil, &v); err != nil {
		resp.Diagnostics.AddError("reading the values", err.Error())
		return
	}
	if len(v.Problems) > 0 {
		resp.Diagnostics.AddError("the environment doesn't resolve", v.Problems[0].Key+": "+v.Problems[0].Problem)
		return
	}
	values, diags := types.MapValueFrom(ctx, types.StringType, v.Values)
	resp.Diagnostics.Append(diags...)
	m.Project, m.Values = types.StringValue(p), values
	resp.Diagnostics.Append(resp.State.Set(ctx, m)...)
}
