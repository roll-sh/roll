// Package provider is the roll Terraform provider: roll secrets' environments, keys and values
// (docs/secrets.md, "Terraform").
package provider

import (
	"context"
	"os"

	"github.com/hashicorp/terraform-plugin-framework/datasource"
	"github.com/hashicorp/terraform-plugin-framework/provider"
	"github.com/hashicorp/terraform-plugin-framework/provider/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/types"
)

// New is the provider at version.
func New(version string) func() provider.Provider {
	return func() provider.Provider { return &rollProvider{version: version} }
}

type rollProvider struct{ version string }

type providerModel struct {
	APIURL  types.String `tfsdk:"api_url"`
	Token   types.String `tfsdk:"token"`
	Project types.String `tfsdk:"project"`
}

func (p *rollProvider) Metadata(_ context.Context, _ provider.MetadataRequest, resp *provider.MetadataResponse) {
	resp.TypeName = "roll"
	resp.Version = p.version
}

func (p *rollProvider) Schema(_ context.Context, _ provider.SchemaRequest, resp *provider.SchemaResponse) {
	resp.Schema = schema.Schema{
		Description: "roll secrets: a project's environments, keys and values. Each setting falls back to the environment variable `roll` reads (ROLL_API_URL, ROLL_TOKEN, ROLL_PROJECT).",
		Attributes: map[string]schema.Attribute{
			"api_url": schema.StringAttribute{Optional: true, Description: "rolld's API, e.g. https://api.roll.example (ROLL_API_URL)."},
			"token":   schema.StringAttribute{Optional: true, Sensitive: true, Description: "A roll token with a rule on the project, or the operator token (ROLL_TOKEN)."},
			"project": schema.StringAttribute{Optional: true, Description: "The default project (prj_…) for resources that don't name one (ROLL_PROJECT)."},
		},
	}
}

func (p *rollProvider) Configure(ctx context.Context, req provider.ConfigureRequest, resp *provider.ConfigureResponse) {
	var m providerModel
	resp.Diagnostics.Append(req.Config.Get(ctx, &m)...)
	if resp.Diagnostics.HasError() {
		return
	}
	pick := func(v types.String, env string) string {
		if !v.IsNull() && !v.IsUnknown() && v.ValueString() != "" {
			return v.ValueString()
		}
		return os.Getenv(env)
	}
	api, token := pick(m.APIURL, "ROLL_API_URL"), pick(m.Token, "ROLL_TOKEN")
	if api == "" || token == "" {
		resp.Diagnostics.AddError("roll isn't configured", "set api_url and token (or ROLL_API_URL and ROLL_TOKEN)")
		return
	}
	c := newClient(api, token, pick(m.Project, "ROLL_PROJECT"))
	resp.ResourceData = c
	resp.DataSourceData = c
}

func (p *rollProvider) Resources(context.Context) []func() resource.Resource {
	return []func() resource.Resource{newEnvironmentResource, newSecretResource, newIdentityResource, newSyncResource}
}

func (p *rollProvider) DataSources(context.Context) []func() datasource.DataSource {
	return []func() datasource.DataSource{newValuesDataSource}
}

// project is a resource's own project, or the provider's.
func project(c *client, own types.String) (string, bool) {
	if !own.IsNull() && !own.IsUnknown() && own.ValueString() != "" {
		return own.ValueString(), true
	}
	return c.project, c.project != ""
}
