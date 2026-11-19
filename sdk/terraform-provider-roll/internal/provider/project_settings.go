package provider

import (
	"context"
	"net/http"
	"net/url"
	"strings"

	"github.com/hashicorp/terraform-plugin-framework/diag"
	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/tfsdk"
	"github.com/hashicorp/terraform-plugin-framework/types"
)

// A project's settings that are one value per project, not a list of items: its required keys and
// its customer-managed key. The resource's id is the project's; an import takes `prj_…`.
type setting[M any] struct {
	c                     *client
	typeName, route, what string
	description           string
	schema                map[string]schema.Attribute
	project               func(*M) (*types.String, *types.String)
	// put is the PUT body; fill reads a GET or PUT answer, false when the setting is unset.
	put  func(ctx context.Context, m *M, d *diag.Diagnostics) any
	fill func(m *M, v any) bool
	// del is how destroy unsets it: a DELETE, or a PUT of this body.
	delPut any
}

func (r *setting[M]) Metadata(_ context.Context, req resource.MetadataRequest, resp *resource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + r.typeName
}

func (r *setting[M]) Schema(_ context.Context, _ resource.SchemaRequest, resp *resource.SchemaResponse) {
	attrs := map[string]schema.Attribute{
		"id":      schema.StringAttribute{Computed: true, Description: "The project's id.", PlanModifiers: []planmodifier.String{stringplanmodifier.UseStateForUnknown()}},
		"project": schema.StringAttribute{Optional: true, Computed: true, PlanModifiers: []planmodifier.String{stringplanmodifier.RequiresReplace(), stringplanmodifier.UseStateForUnknown()}, Description: "prj_…; the provider's project when left out."},
	}
	for k, v := range r.schema {
		attrs[k] = v
	}
	resp.Schema = schema.Schema{Description: r.description, Attributes: attrs}
}

func (r *setting[M]) Configure(_ context.Context, req resource.ConfigureRequest, _ *resource.ConfigureResponse) {
	if c, ok := req.ProviderData.(*client); ok {
		r.c = c
	}
}

func (r *setting[M]) at(p string) string {
	return "/v1/secrets/projects/" + url.PathEscape(p) + "/" + r.route
}

func (r *setting[M]) write(ctx context.Context, plan tfsdk.Plan, state *tfsdk.State, d *diag.Diagnostics) {
	var m M
	d.Append(plan.Get(ctx, &m)...)
	proj, id := r.project(&m)
	p, ok := project(r.c, *proj)
	if !ok {
		d.AddError("which project?", "set project here or on the provider (ROLL_PROJECT)")
	}
	if d.HasError() {
		return
	}
	body := r.put(ctx, &m, d)
	if d.HasError() {
		return
	}
	var v any
	if err := r.c.call(ctx, http.MethodPut, r.at(p), body, &v); err != nil {
		d.AddError("setting the "+r.what, err.Error())
		return
	}
	*proj, *id = types.StringValue(p), types.StringValue(p)
	r.fill(&m, v)
	d.Append(state.Set(ctx, m)...)
}

func (r *setting[M]) Create(ctx context.Context, req resource.CreateRequest, resp *resource.CreateResponse) {
	r.write(ctx, req.Plan, &resp.State, &resp.Diagnostics)
}

func (r *setting[M]) Update(ctx context.Context, req resource.UpdateRequest, resp *resource.UpdateResponse) {
	r.write(ctx, req.Plan, &resp.State, &resp.Diagnostics)
}

func (r *setting[M]) Read(ctx context.Context, req resource.ReadRequest, resp *resource.ReadResponse) {
	var m M
	resp.Diagnostics.Append(req.State.Get(ctx, &m)...)
	if resp.Diagnostics.HasError() {
		return
	}
	proj, _ := r.project(&m)
	var v any
	if err := r.c.call(ctx, http.MethodGet, r.at(proj.ValueString()), nil, &v); err != nil {
		if notFound(err) {
			resp.State.RemoveResource(ctx)
			return
		}
		resp.Diagnostics.AddError("reading the "+r.what, err.Error())
		return
	}
	if !r.fill(&m, v) {
		resp.State.RemoveResource(ctx)
		return
	}
	resp.Diagnostics.Append(resp.State.Set(ctx, m)...)
}

func (r *setting[M]) Delete(ctx context.Context, req resource.DeleteRequest, resp *resource.DeleteResponse) {
	var m M
	resp.Diagnostics.Append(req.State.Get(ctx, &m)...)
	if resp.Diagnostics.HasError() {
		return
	}
	proj, _ := r.project(&m)
	method := http.MethodDelete
	if r.delPut != nil {
		method = http.MethodPut
	}
	if err := r.c.call(ctx, method, r.at(proj.ValueString()), r.delPut, nil); err != nil && !notFound(err) {
		resp.Diagnostics.AddError("unsetting the "+r.what, err.Error())
	}
}

func (r *setting[M]) ImportState(ctx context.Context, req resource.ImportStateRequest, resp *resource.ImportStateResponse) {
	importProject(ctx, r.c, req, resp, "prj_…", false)
}

// roll_secrets_required_keys: the project's whole list of required keys.
type requiredKey struct {
	Key     types.String `tfsdk:"key"`
	Path    types.String `tfsdk:"path"`
	Comment types.String `tfsdk:"comment"`
	Type    types.String `tfsdk:"type"`
	Pattern types.String `tfsdk:"pattern"`
	Owner   types.String `tfsdk:"owner"`
}

type requiredKeysModel struct {
	ID      types.String  `tfsdk:"id"`
	Project types.String  `tfsdk:"project"`
	Keys    []requiredKey `tfsdk:"keys"`
}

func newRequiredKeysResource() resource.Resource {
	opt := func(desc string) schema.StringAttribute {
		return schema.StringAttribute{Optional: true, Description: desc}
	}
	return &setting[requiredKeysModel]{
		typeName: "_secrets_required_keys", route: "required", what: "required keys",
		description: "The keys every standard environment of the project must define (docs/secrets.md, \"Required keys\"). One resource per project owns the whole list: a key left out of it is no longer required, and two of these on one project undo each other. Destroying it requires nothing.",
		schema: map[string]schema.Attribute{
			"keys": schema.SetNestedAttribute{Required: true, Description: "Each required key, in a folder (500 at most).",
				NestedObject: schema.NestedAttributeObject{Attributes: map[string]schema.Attribute{
					"key":     schema.StringAttribute{Required: true, Description: "The key's name."},
					"path":    opt("Its folder: / (when left out) or /folder/…, without a trailing / (an import reads it so)."),
					"comment": opt("What it's for."),
					"type":    opt("What its value must be: url, int, port, bool, json or email."),
					"pattern": opt("A regular expression the whole value must match."),
					"owner":   opt("Who supplies it (a person or team, @payments): health and the daily notice name them."),
				}}},
		},
		project: func(m *requiredKeysModel) (*types.String, *types.String) { return &m.Project, &m.ID },
		put: func(_ context.Context, m *requiredKeysModel, _ *diag.Diagnostics) any {
			list := make([]map[string]any, 0, len(m.Keys))
			for _, k := range m.Keys {
				r := map[string]any{"key": k.Key.ValueString()}
				for name, v := range map[string]types.String{"path": k.Path, "comment": k.Comment, "type": k.Type, "pattern": k.Pattern, "owner": k.Owner} {
					if !v.IsNull() && !v.IsUnknown() {
						r[name] = v.ValueString()
					}
				}
				list = append(list, r)
			}
			return list
		},
		// roll's answer, each element as configured (m's) when it's the same key normalised: a
		// folder left out is /, a trailing / goes, an empty field is left out.
		fill: func(m *requiredKeysModel, v any) bool {
			norm := func(path bool, v types.String) string {
				s := v.ValueString()
				if path && (s == "" || s == "/") {
					return "/"
				}
				if path {
					return strings.TrimSuffix(s, "/")
				}
				return s
			}
			field := func(path bool, was, got types.String) types.String {
				if norm(path, was) == norm(path, got) && !was.IsUnknown() {
					return was
				}
				if s := got.ValueString(); s == "" || path && s == "/" {
					return types.StringNull()
				}
				return got
			}
			prior := map[string]requiredKey{}
			for _, k := range m.Keys {
				prior[norm(true, k.Path)+" "+k.Key.ValueString()] = k
			}
			xs, _ := v.([]any)
			m.Keys = make([]requiredKey, 0, len(xs))
			for _, x := range xs {
				r, _ := x.(map[string]any)
				got := requiredKey{Key: jStr(r, "key"), Path: jStr(r, "path"), Comment: jStr(r, "comment"), Type: jStr(r, "type"), Pattern: jStr(r, "pattern"), Owner: jStr(r, "owner")}
				was := prior[norm(true, got.Path)+" "+got.Key.ValueString()]
				m.Keys = append(m.Keys, requiredKey{Key: got.Key, Path: field(true, was.Path, got.Path), Comment: field(false, was.Comment, got.Comment),
					Type: field(false, was.Type, got.Type), Pattern: field(false, was.Pattern, got.Pattern), Owner: field(false, was.Owner, got.Owner)})
			}
			return true
		},
		delPut: []any{},
	}
}

// roll_secrets_kms_key: the customer's KMS key that wraps the project's data key.
type kmsKeyModel struct {
	ID      types.String `tfsdk:"id"`
	Project types.String `tfsdk:"project"`
	Key     types.String `tfsdk:"key"`
}

func newKmsKeyResource() resource.Resource {
	return &setting[kmsKeyModel]{
		typeName: "_secrets_kms_key", route: "kms", what: "customer-managed key",
		description: "A customer-managed key (docs/secrets.md, \"Customer-managed keys\"): a KMS key in the customer's AWS account wraps the project's data key, so revoking roll's grant makes the project unreadable to roll. Setting one checks roll can use it (and that it's bound to this project). A new key changes in place; destroying it unwraps the data key (KMS must still answer).",
		schema: map[string]schema.Attribute{
			"key": schema.StringAttribute{Required: true, Description: "The KMS key's ARN (arn:aws:kms:<region>:<account>:key/<id>)."},
		},
		project: func(m *kmsKeyModel) (*types.String, *types.String) { return &m.Project, &m.ID },
		put: func(_ context.Context, m *kmsKeyModel, _ *diag.Diagnostics) any {
			return map[string]any{"key": m.Key.ValueString()}
		},
		fill: func(m *kmsKeyModel, v any) bool {
			o, _ := v.(map[string]any)
			m.Key = jStr(o, "key")
			return !m.Key.IsNull()
		},
	}
}
