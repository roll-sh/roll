package provider

import (
	"context"
	"net/http"
	"net/url"

	"github.com/hashicorp/terraform-plugin-framework-validators/objectvalidator"
	"github.com/hashicorp/terraform-plugin-framework/diag"
	"github.com/hashicorp/terraform-plugin-framework/path"
	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringdefault"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/schema/validator"
	"github.com/hashicorp/terraform-plugin-framework/types"
)

// roll_secret: one key in an environment's folder, either a value (sensitive; read back with
// reveal, so a change made outside Terraform shows in the plan) or a link to a roll resource.
type secretResource struct{ c *client }

type linkModel struct {
	Kind  types.String `tfsdk:"kind"`
	Ref   types.String `tfsdk:"ref"`
	Field types.String `tfsdk:"field"`
	Mode  types.String `tfsdk:"mode"`
}

type secretModel struct {
	ID          types.String `tfsdk:"id"`
	Project     types.String `tfsdk:"project"`
	Environment types.String `tfsdk:"environment"`
	Path        types.String `tfsdk:"path"`
	Key         types.String `tfsdk:"key"`
	Value       types.String `tfsdk:"value"`
	Comment     types.String `tfsdk:"comment"`
	Link        *linkModel   `tfsdk:"link"`
	Version     types.Int64  `tfsdk:"version"`
}

func newSecretResource() resource.Resource { return &secretResource{} }

func (r *secretResource) Metadata(_ context.Context, req resource.MetadataRequest, resp *resource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_secret"
}

func (r *secretResource) Schema(_ context.Context, _ resource.SchemaRequest, resp *resource.SchemaResponse) {
	replace := []planmodifier.String{stringplanmodifier.RequiresReplace()}
	resp.Schema = schema.Schema{
		Description: "A key in an environment: a value, or a link to a roll resource (a database's URL, a bucket's key). In a protected environment a write opens a change request instead, which Terraform reports as an error.",
		Attributes: map[string]schema.Attribute{
			"id":          schema.StringAttribute{Computed: true, PlanModifiers: []planmodifier.String{stringplanmodifier.UseStateForUnknown()}},
			"project":     schema.StringAttribute{Optional: true, Computed: true, PlanModifiers: append(replace, stringplanmodifier.UseStateForUnknown()), Description: "prj_…; the provider's project when left out."},
			"environment": schema.StringAttribute{Required: true, PlanModifiers: replace, Description: "The environment's slug."},
			"path":        schema.StringAttribute{Optional: true, Computed: true, Default: stringdefault.StaticString("/"), PlanModifiers: replace, Description: "The folder (default /)."},
			"key":         schema.StringAttribute{Required: true, PlanModifiers: replace},
			"value":       schema.StringAttribute{Optional: true, Sensitive: true, Description: "The value. It may hold references (${staging.DB_HOST}); they're kept as written."},
			"comment":     schema.StringAttribute{Optional: true, Computed: true, Default: stringdefault.StaticString("")},
			"link": schema.SingleNestedAttribute{
				Optional:    true,
				Description: "A link to a roll resource on the project's canvas instead of a value.",
				Attributes: map[string]schema.Attribute{
					"kind":  schema.StringAttribute{Required: true, Description: "postgres, redis, bucket, stream, nosql, search, rotator…"},
					"ref":   schema.StringAttribute{Required: true, Description: "The resource's id."},
					"field": schema.StringAttribute{Required: true, Description: "url, password, access_key…"},
					"mode":  schema.StringAttribute{Optional: true, Computed: true, Default: stringdefault.StaticString("live"), Description: "live, managed or dynamic."},
				},
				Validators: []validator.Object{objectvalidator.ConflictsWith(path.MatchRoot("value"))},
			},
			"version": schema.Int64Attribute{Computed: true, Description: "Its version in roll secrets (a write makes a new one)."},
		},
	}
}

func (r *secretResource) Configure(_ context.Context, req resource.ConfigureRequest, _ *resource.ConfigureResponse) {
	if c, ok := req.ProviderData.(*client); ok {
		r.c = c
	}
}

func (r *secretResource) url(m *secretModel, suffix string) string {
	return r.c.envPath(m.Project.ValueString(), m.Environment.ValueString()) + "/secrets/" + url.PathEscape(m.Key.ValueString()) + suffix + pathQuery(m.Path.ValueString())
}

func (r *secretResource) write(ctx context.Context, m *secretModel, d *diag.Diagnostics) bool {
	p, ok := project(r.c, m.Project)
	if !ok {
		d.AddError("which project?", "set project here or on the provider (ROLL_PROJECT)")
		return false
	}
	m.Project = types.StringValue(p)
	body := map[string]any{"comment": m.Comment.ValueString()}
	if m.Link != nil {
		body["link"] = link{Kind: m.Link.Kind.ValueString(), Ref: m.Link.Ref.ValueString(), Field: m.Link.Field.ValueString(), Mode: m.Link.Mode.ValueString()}
	} else {
		body["value"] = m.Value.ValueString()
		body["link"] = nil
	}
	var w struct {
		Warnings []string `json:"warnings"`
	}
	if err := r.c.call(ctx, http.MethodPut, r.url(m, ""), body, &w); err != nil {
		d.AddError("writing the key", err.Error())
		return false
	}
	for _, warning := range w.Warnings {
		d.AddWarning("roll secrets", warning)
	}
	if err := r.read(ctx, m); err != nil {
		d.AddError("reading the key back", err.Error())
		return false
	}
	return true
}

// read fills m from roll: gone (or only inherited from a base or an import) is a 404.
func (r *secretResource) read(ctx context.Context, m *secretModel) error {
	var s secret
	if err := r.c.call(ctx, http.MethodGet, r.url(m, ""), nil, &s); err != nil {
		return err
	}
	if s.Source.Kind != "" && s.Source.Kind != "own" {
		return &apiError{status: http.StatusNotFound, message: "only inherited"}
	}
	m.ID = types.StringValue(m.Project.ValueString() + "/" + m.Environment.ValueString() + ":" + s.Path + "/" + s.Key)
	m.Comment, m.Version = types.StringValue(s.Comment), types.Int64Value(s.Version)
	if s.Link != nil {
		m.Link = &linkModel{Kind: types.StringValue(s.Link.Kind), Ref: types.StringValue(s.Link.Ref), Field: types.StringValue(s.Link.Field), Mode: types.StringValue(s.Link.Mode)}
		m.Value = types.StringNull()
		return nil
	}
	m.Link = nil
	// The stored value, references as written (reveal: the token must be allowed to).
	var v struct {
		Value string `json:"value"`
	}
	if err := r.c.call(ctx, http.MethodGet, r.url(m, "/value")+"&resolve=false", nil, &v); err != nil {
		return err
	}
	m.Value = types.StringValue(v.Value)
	return nil
}

func (r *secretResource) Create(ctx context.Context, req resource.CreateRequest, resp *resource.CreateResponse) {
	var m secretModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &m)...)
	if resp.Diagnostics.HasError() || !r.write(ctx, &m, &resp.Diagnostics) {
		return
	}
	resp.Diagnostics.Append(resp.State.Set(ctx, m)...)
}

func (r *secretResource) Read(ctx context.Context, req resource.ReadRequest, resp *resource.ReadResponse) {
	var m secretModel
	resp.Diagnostics.Append(req.State.Get(ctx, &m)...)
	if resp.Diagnostics.HasError() {
		return
	}
	if err := r.read(ctx, &m); notFound(err) {
		resp.State.RemoveResource(ctx)
		return
	} else if err != nil {
		resp.Diagnostics.AddError("reading the key", err.Error())
		return
	}
	resp.Diagnostics.Append(resp.State.Set(ctx, m)...)
}

func (r *secretResource) Update(ctx context.Context, req resource.UpdateRequest, resp *resource.UpdateResponse) {
	var m secretModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &m)...)
	if resp.Diagnostics.HasError() || !r.write(ctx, &m, &resp.Diagnostics) {
		return
	}
	resp.Diagnostics.Append(resp.State.Set(ctx, m)...)
}

func (r *secretResource) Delete(ctx context.Context, req resource.DeleteRequest, resp *resource.DeleteResponse) {
	var m secretModel
	resp.Diagnostics.Append(req.State.Get(ctx, &m)...)
	if resp.Diagnostics.HasError() {
		return
	}
	if err := r.c.call(ctx, http.MethodDelete, r.url(&m, ""), nil, nil); err != nil && !notFound(err) {
		resp.Diagnostics.AddError("deleting the key", err.Error())
	}
}
