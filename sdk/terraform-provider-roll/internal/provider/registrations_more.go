package provider

import (
	"context"
	"net/http"

	"github.com/hashicorp/terraform-plugin-framework-validators/stringvalidator"
	"github.com/hashicorp/terraform-plugin-framework/attr"
	"github.com/hashicorp/terraform-plugin-framework/diag"
	"github.com/hashicorp/terraform-plugin-framework/path"
	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/booldefault"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/boolplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/int64planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/mapplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringdefault"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/schema/validator"
	"github.com/hashicorp/terraform-plugin-framework/types"
)

// More of what a project registers itself: rotators, audit sinks, GitHub Apps, AWS roles and IAM
// users, GCP and Azure accounts. Credentials going in (a private key, a SIEM's token) are
// write-only; a signing secret roll shows once is kept, sensitive, from the answer that showed it.

// An attribute roll changes in place (a PATCH).
func inPlace(desc string) schema.StringAttribute {
	return schema.StringAttribute{Required: true, Description: desc}
}

func reqInt(desc string) schema.Int64Attribute {
	return schema.Int64Attribute{Required: true, Description: desc, PlanModifiers: []planmodifier.Int64{int64planmodifier.RequiresReplace()}}
}

// A computed attribute that stays as it is unless one of `when` changes (then it's known after
// apply): a host follows its URL, a signing secret its rotate trigger.
func followsString(desc string, sensitive bool, when ...string) schema.StringAttribute {
	return schema.StringAttribute{Computed: true, Sensitive: sensitive, Description: desc, PlanModifiers: []planmodifier.String{follows(when)}}
}

type follows []string

func (f follows) Description(context.Context) string {
	return "kept from the state unless what it follows changes"
}
func (f follows) MarkdownDescription(ctx context.Context) string { return f.Description(ctx) }

func (f follows) PlanModifyString(ctx context.Context, req planmodifier.StringRequest, resp *planmodifier.StringResponse) {
	if req.State.Raw.IsNull() || !req.PlanValue.IsUnknown() {
		return
	}
	for _, k := range f {
		var was, will attr.Value
		resp.Diagnostics.Append(req.State.GetAttribute(ctx, path.Root(k), &was)...)
		resp.Diagnostics.Append(req.Plan.GetAttribute(ctx, path.Root(k), &will)...)
		if !will.Equal(was) {
			return
		}
	}
	resp.PlanValue = req.StateValue
}

func jMap(v map[string]any, k string, keepNull types.Map) types.Map {
	m, _ := v[k].(map[string]any)
	if len(m) == 0 && keepNull.IsNull() {
		return types.MapNull(types.StringType)
	}
	vs := map[string]attr.Value{}
	for k, x := range m {
		if s, ok := x.(string); ok {
			vs[k] = types.StringValue(s)
		}
	}
	return types.MapValueMust(types.StringType, vs)
}

// changed puts k: plan's value in body when it differs from prior's.
func changed(body map[string]any, k string, plan, prior types.String) {
	if !plan.Equal(prior) && !plan.IsUnknown() {
		body[k] = plan.ValueString()
	}
}

// patch sends body (when there's anything in it) and fills m from the answer.
func patch[M any](ctx context.Context, c *client, at string, body map[string]any, m *M, fill func(*M, map[string]any), what string, d *diag.Diagnostics) {
	if len(body) == 0 {
		return
	}
	var v map[string]any
	if err := c.call(ctx, http.MethodPatch, at, body, &v); err != nil {
		d.AddError("updating the "+what, err.Error())
		return
	}
	fill(m, v)
}

// unset is a configured value that's absent (null); unknown counts as set.
func unset(v attr.Value) bool { return v.IsNull() }

// roll_secrets_rotator: a URL roll calls to issue and revoke a value that lives outside roll.
type rotatorModel struct {
	ID           types.String `tfsdk:"id"`
	Project      types.String `tfsdk:"project"`
	Name         types.String `tfsdk:"name"`
	URL          types.String `tfsdk:"url"`
	URLWO        types.String `tfsdk:"url_wo"`
	URLWOVersion types.Int64  `tfsdk:"url_wo_version"`
	Host         types.String `tfsdk:"host"`
	Secret       types.String `tfsdk:"secret"`
	RotateSecret types.Map    `tfsdk:"rotate_secret"`
}

func fillRotator(m *rotatorModel, v map[string]any) {
	m.ID, m.Name, m.Host, m.URLWO = jStr(v, "id"), jStr(v, "name"), jStr(v, "host"), types.StringNull()
	// Shown only by the answers that make one.
	if s := jStr(v, "secret"); !s.IsNull() {
		m.Secret = s
	}
}

func newRotatorResource() resource.Resource {
	return &registration[rotatorModel]{
		typeName: "_secrets_rotator", route: "rotators", what: "rotator",
		description: "A rotator (docs/secrets.md, \"Rotators\"): a URL roll calls, signed, to issue and revoke a value that lives outside roll. Link keys to it (`kind = \"rotator\"`, `mode = \"managed\"`). Its name and URL change in place. A URL that carries a token goes in url_wo (write-only, Terraform 1.11+).",
		schema: map[string]schema.Attribute{
			"name": inPlace("Its name: letters, digits, - and _."),
			"url": schema.StringAttribute{Optional: true, Sensitive: true,
				Description: "The HTTPS endpoint roll calls, when it carries no secret: it's in the plan and the state, as configured. roll seals it and shows only its host, and never reads it back, so a change made outside Terraform is put back only when this changes. A URL with a token in it goes in url_wo instead."},
			"url_wo": schema.StringAttribute{Optional: true, Sensitive: true, WriteOnly: true,
				Description: "The endpoint, when it carries a token (in its query or path): write-only, so it's sent to roll (sealed there, only its host shown) and never in the plan or the state. Change url_wo_version to send a new one; the rotator changes in place.",
				Validators:  []validator.String{stringvalidator.AlsoRequires(path.MatchRoot("url_wo_version")), stringvalidator.ConflictsWith(path.MatchRoot("url"))}},
			"url_wo_version": schema.Int64Attribute{Optional: true, Description: "Bump it to send url_wo again (in place)."},
			"host":           followsString("The URL's host, as roll shows it.", false, "url", "url_wo_version"),
			"secret":         followsString("The signing secret (whsec_…) roll signs every call with: shown once, so it's kept from the answer that made it (creating the rotator, or a rotate_secret change). Not imported.", true, "rotate_secret"),
			"rotate_secret": schema.MapAttribute{Optional: true, ElementType: types.StringType,
				Description: "Any values (a keepers map): changing them rotates the signing secret in place (the new one is in secret, and signs every call from then on)."},
		},
		keys: func(m *rotatorModel) (*types.String, *types.String) { return &m.Project, &m.ID },
		validate: func(_ context.Context, m *rotatorModel, d *diag.Diagnostics) {
			if unset(m.URL) && unset(m.URLWO) {
				d.AddAttributeError(path.Root("url"), "a rotator needs its URL", "set url, or url_wo (and url_wo_version) when the URL carries a token.")
			}
			if !unset(m.URLWOVersion) && unset(m.URLWO) {
				d.AddAttributeError(path.Root("url_wo_version"), "url_wo_version goes with url_wo", "set url_wo, or drop url_wo_version.")
			}
		},
		body: func(_ context.Context, m *rotatorModel, cfg func(string) types.String, _ *diag.Diagnostics) map[string]any {
			u := m.URL
			if u.IsNull() {
				u = cfg("url_wo")
			}
			return map[string]any{"name": m.Name.ValueString(), "url": u.ValueString()}
		},
		fill: fillRotator,
		update: func(ctx context.Context, c *client, at string, plan, prior *rotatorModel, cfg func(string) types.String, d *diag.Diagnostics) {
			body := map[string]any{}
			changed(body, "name", plan.Name, prior.Name)
			if plan.URL.IsNull() {
				// A write-only URL is sent when its version changes (or it replaces a plain one).
				if !plan.URLWOVersion.Equal(prior.URLWOVersion) || !prior.URL.IsNull() {
					body["url"] = cfg("url_wo").ValueString()
				}
			} else {
				changed(body, "url", plan.URL, prior.URL)
			}
			plan.URLWO = types.StringNull()
			patch(ctx, c, at, body, plan, fillRotator, "rotator", d)
			if d.HasError() || plan.RotateSecret.Equal(prior.RotateSecret) {
				return
			}
			var v map[string]any
			if err := c.call(ctx, http.MethodPost, at+"/rotate-secret", nil, &v); err != nil {
				d.AddError("rotating the rotator's signing secret", err.Error())
				return
			}
			fillRotator(plan, v)
		},
	}
}

// roll_secrets_audit_sink: the project's audit log, streamed to a SIEM or a roll resource.
type auditSinkModel struct {
	ID             types.String `tfsdk:"id"`
	Project        types.String `tfsdk:"project"`
	URL            types.String `tfsdk:"url"`
	Format         types.String `tfsdk:"format"`
	FromStart      types.Bool   `tfsdk:"from_start"`
	TokenWO        types.String `tfsdk:"token_wo"`
	TokenWOVersion types.Int64  `tfsdk:"token_wo_version"`
	Secret         types.String `tfsdk:"secret"`
}

func newAuditSinkResource() resource.Resource {
	return &registration[auditSinkModel]{
		typeName: "_secrets_audit_sink", route: "audit/sinks", what: "audit sink",
		description: "An audit sink (docs/secrets.md, \"Streaming to a SIEM\"): the project's new audit rows, in order, to an HTTPS endpoint or a roll resource on the canvas. A SIEM format's token is write-only (Terraform 1.11+). roll changes none in place: every attribute replaces it.",
		schema: map[string]schema.Attribute{
			"url": reqString("An HTTPS endpoint, or on the canvas bucket:<name>, search:<cluster>/<index> or stream:<id>/<topic>."),
			"format": schema.StringAttribute{Optional: true, Computed: true, Default: stringdefault.StaticString("roll"),
				Description:   "roll (signed JSON, the default), or a SIEM's own: splunk (HTTP Event Collector), datadog (the Logs intake) or bearer; those take an HTTPS endpoint and token_wo.",
				Validators:    []validator.String{stringvalidator.OneOf("roll", "splunk", "datadog", "bearer")},
				PlanModifiers: []planmodifier.String{stringplanmodifier.RequiresReplace()}},
			"from_start": schema.BoolAttribute{Optional: true, Computed: true, Default: booldefault.StaticBool(false),
				Description:   "Deliver every row the project still has, not only those from now on (roll doesn't show it again: an import reads false).",
				PlanModifiers: []planmodifier.Bool{boolplanmodifier.RequiresReplace()}},
			"token_wo": schema.StringAttribute{Optional: true, Sensitive: true, WriteOnly: true,
				Description: "The SIEM's token (Splunk's HEC token, Datadog's API key, a bearer token), for a format other than roll. Write-only: sealed in roll, never shown again, never in the plan or the state. Change token_wo_version to send a new one (it replaces the sink).",
				Validators:  []validator.String{stringvalidator.AlsoRequires(path.MatchRoot("token_wo_version"))}},
			"token_wo_version": schema.Int64Attribute{Optional: true, Description: "Bump it to make the sink again with a new token_wo.",
				PlanModifiers: []planmodifier.Int64{int64planmodifier.RequiresReplace()}},
			"secret": outSecret("The signing secret (whsec_…) a roll-format sink's batches are signed with: shown once, when the sink is made. Not imported."),
		},
		keys: func(m *auditSinkModel) (*types.String, *types.String) { return &m.Project, &m.ID },
		validate: func(_ context.Context, m *auditSinkModel, d *diag.Diagnostics) {
			roll := m.Format.IsNull() || m.Format.ValueString() == "roll"
			if m.Format.IsUnknown() {
				return
			}
			if roll && !unset(m.TokenWO) {
				d.AddAttributeError(path.Root("token_wo"), "a roll sink takes no token", "roll signs its batches (secret); token_wo is for format splunk, datadog or bearer.")
			}
			if !roll && unset(m.TokenWO) {
				d.AddAttributeError(path.Root("token_wo"), "a "+m.Format.ValueString()+" sink needs its token", "set token_wo (and token_wo_version).")
			}
		},
		body: func(_ context.Context, m *auditSinkModel, cfg func(string) types.String, _ *diag.Diagnostics) map[string]any {
			b := map[string]any{"url": m.URL.ValueString(), "from_start": m.FromStart.ValueBool()}
			if f := m.Format.ValueString(); f != "" && f != "roll" {
				b["format"], b["token"] = f, cfg("token_wo").ValueString()
			}
			return b
		},
		fill: func(m *auditSinkModel, v map[string]any) {
			m.ID, m.URL, m.TokenWO = jStr(v, "id"), sameString(m.URL, jStr(v, "url")), types.StringNull()
			if f := jStr(v, "format"); !f.IsNull() {
				m.Format = f
			}
			if m.FromStart.IsNull() || m.FromStart.IsUnknown() {
				m.FromStart = types.BoolValue(false)
			}
			if s := jStr(v, "secret"); !s.IsNull() {
				m.Secret = s
			} else if m.Secret.IsUnknown() {
				m.Secret = types.StringNull()
			}
		},
	}
}

// A sensitive answer shown once, kept from it.
func outSecret(desc string) schema.StringAttribute {
	return schema.StringAttribute{Computed: true, Sensitive: true, Description: desc, PlanModifiers: []planmodifier.String{stringplanmodifier.UseStateForUnknown()}}
}

// roll_secrets_github_app: a GitHub App's installation, for short-lived GitHub tokens.
type githubAppModel struct {
	ID                  types.String `tfsdk:"id"`
	Project             types.String `tfsdk:"project"`
	Name                types.String `tfsdk:"name"`
	AppID               types.Int64  `tfsdk:"app_id"`
	InstallationID      types.Int64  `tfsdk:"installation_id"`
	PrivateKeyWO        types.String `tfsdk:"private_key_wo"`
	PrivateKeyWOVersion types.Int64  `tfsdk:"private_key_wo_version"`
	Repositories        types.List   `tfsdk:"repositories"`
	Permissions         types.Map    `tfsdk:"permissions"`
	APIBase             types.String `tfsdk:"api_base"`
}

func newGitHubAppResource() resource.Resource {
	return &registration[githubAppModel]{
		typeName: "_secrets_github_app", route: "github-apps", what: "GitHub App",
		description: "A GitHub App's installation (docs/secrets.md, \"GitHub App tokens\"): keys link to it (`kind = \"github_app\"`, field `token`, `dynamic`) for a short-lived token per lease, narrowed within repositories and permissions. Its private key is write-only (Terraform 1.11+): sealed in roll, never shown again, never in the state. roll changes none in place: every attribute replaces it.",
		schema: map[string]schema.Attribute{
			"name":            reqString("Its name."),
			"app_id":          reqInt("The App's id."),
			"installation_id": reqInt("The installation's id (the account or organization it's installed on)."),
			"private_key_wo": schema.StringAttribute{Required: true, Sensitive: true, WriteOnly: true,
				Description: "The App's private key (PEM), as GitHub gave it. Write-only: change private_key_wo_version to register the App again with a new key."},
			"private_key_wo_version": reqInt("Bump it to register the App again with a new private_key_wo (it replaces the registration)."),
			"repositories":           strList(true, "The repositories (of the installation's owner) a token may reach at most."),
			"permissions": schema.MapAttribute{Required: true, ElementType: types.StringType,
				Description:   "The most a token may have: permission → read, write or admin ({ contents = \"read\" }).",
				PlanModifiers: []planmodifier.Map{mapplanmodifier.RequiresReplace()}},
			"api_base": schema.StringAttribute{Optional: true, Computed: true, Default: stringdefault.StaticString("https://api.github.com"),
				Description:   "A GitHub Enterprise Server's API (https://github.example.com/api/v3); github.com's by default.",
				PlanModifiers: []planmodifier.String{stringplanmodifier.RequiresReplace()}},
		},
		keys: func(m *githubAppModel) (*types.String, *types.String) { return &m.Project, &m.ID },
		body: func(ctx context.Context, m *githubAppModel, cfg func(string) types.String, d *diag.Diagnostics) map[string]any {
			perms := map[string]string{}
			d.Append(m.Permissions.ElementsAs(ctx, &perms, false)...)
			return map[string]any{"name": m.Name.ValueString(), "app_id": m.AppID.ValueInt64(), "installation_id": m.InstallationID.ValueInt64(),
				"private_key": cfg("private_key_wo").ValueString(), "repositories": stringList(ctx, m.Repositories, d), "permissions": perms, "api_base": m.APIBase.ValueString()}
		},
		fill: func(m *githubAppModel, v map[string]any) {
			m.ID, m.Name, m.AppID, m.InstallationID, m.PrivateKeyWO = jStr(v, "id"), jStr(v, "name"), jInt(v, "app_id"), jInt(v, "installation_id"), types.StringNull()
			m.APIBase = sameString(m.APIBase, jStr(v, "api_base"))
			m.Repositories, m.Permissions = jList(v, "repositories", m.Repositories), jMap(v, "permissions", m.Permissions)
		},
	}
}

// roll_secrets_aws_role: an AWS role roll assumes for short-lived credentials.
type awsRoleModel struct {
	ID      types.String `tfsdk:"id"`
	Project types.String `tfsdk:"project"`
	Name    types.String `tfsdk:"name"`
	RoleARN types.String `tfsdk:"role_arn"`
	Trust   types.String `tfsdk:"trust"`
}

func fillAwsRole(m *awsRoleModel, v map[string]any) {
	m.ID, m.Name, m.RoleARN, m.Trust = jStr(v, "id"), jStr(v, "name"), jStr(v, "role_arn"), jStr(v, "trust")
}

func newAwsRoleResource() resource.Resource {
	return &registration[awsRoleModel]{
		typeName: "_secrets_aws_role", route: "aws-roles", what: "AWS role",
		description: "An AWS role in the customer's account (docs/secrets.md, \"AWS\"): keys link to it (`kind = \"aws\"`, `dynamic`) for a session per lease. roll assumes it once when it's registered or changed. Its name, ARN and trust change in place.",
		schema: map[string]schema.Attribute{
			"name":     inPlace("Its name."),
			"role_arn": inPlace("arn:aws:iam::<account>:role/<name>."),
			"trust": schema.StringAttribute{Optional: true, Computed: true, Default: stringdefault.StaticString("external_id"),
				Description: "How the role trusts roll: external_id (roll's AWS role, with sts:ExternalId = roll-secrets:<project>; the default) or oidc (roll's issuer: no AWS credential of roll's).",
				Validators:  []validator.String{stringvalidator.OneOf("external_id", "oidc")}},
		},
		keys: func(m *awsRoleModel) (*types.String, *types.String) { return &m.Project, &m.ID },
		body: func(_ context.Context, m *awsRoleModel, _ func(string) types.String, _ *diag.Diagnostics) map[string]any {
			return map[string]any{"name": m.Name.ValueString(), "role_arn": m.RoleARN.ValueString(), "trust": m.Trust.ValueString()}
		},
		fill: fillAwsRole,
		update: func(ctx context.Context, c *client, at string, plan, prior *awsRoleModel, _ func(string) types.String, d *diag.Diagnostics) {
			body := map[string]any{}
			changed(body, "name", plan.Name, prior.Name)
			changed(body, "role_arn", plan.RoleARN, prior.RoleARN)
			changed(body, "trust", plan.Trust, prior.Trust)
			patch(ctx, c, at, body, plan, fillAwsRole, "AWS role", d)
		},
	}
}

// roll_secrets_aws_user: an IAM user whose access keys roll rotates, through one of the roles.
type awsUserModel struct {
	ID      types.String `tfsdk:"id"`
	Project types.String `tfsdk:"project"`
	Name    types.String `tfsdk:"name"`
	Role    types.String `tfsdk:"role"`
	User    types.String `tfsdk:"user"`
}

func fillAwsUser(m *awsUserModel, v map[string]any) {
	m.ID, m.Name, m.Role, m.User = jStr(v, "id"), jStr(v, "name"), jStr(v, "role"), jStr(v, "user")
}

func newAwsUserResource() resource.Resource {
	return &registration[awsUserModel]{
		typeName: "_secrets_aws_user", route: "aws-users", what: "AWS IAM user",
		description: "An IAM user whose access keys roll rotates (docs/secrets.md, \"AWS\"), acting as one of the project's ExternalId roles; keys link to it (`kind = \"aws_iam_user\"`, `managed`). Its name and role change in place; another user replaces it.",
		schema: map[string]schema.Attribute{
			"name": inPlace("Its name."),
			"role": inPlace("The roll_secrets_aws_role (awr_…) roll acts as: it may list, create and delete the user's access keys."),
			"user": reqString("The IAM user's name."),
		},
		keys: func(m *awsUserModel) (*types.String, *types.String) { return &m.Project, &m.ID },
		body: func(_ context.Context, m *awsUserModel, _ func(string) types.String, _ *diag.Diagnostics) map[string]any {
			return map[string]any{"name": m.Name.ValueString(), "role": m.Role.ValueString(), "user": m.User.ValueString()}
		},
		fill: fillAwsUser,
		update: func(ctx context.Context, c *client, at string, plan, prior *awsUserModel, _ func(string) types.String, d *diag.Diagnostics) {
			body := map[string]any{}
			changed(body, "name", plan.Name, prior.Name)
			changed(body, "role", plan.Role, prior.Role)
			patch(ctx, c, at, body, plan, fillAwsUser, "AWS IAM user", d)
		},
	}
}

// roll_secrets_cloud_account: a GCP service account or an Azure app that trusts roll's issuer.
type cloudAccountModel struct {
	ID             types.String `tfsdk:"id"`
	Project        types.String `tfsdk:"project"`
	Name           types.String `tfsdk:"name"`
	Cloud          types.String `tfsdk:"cloud"`
	Audience       types.String `tfsdk:"audience"`
	ServiceAccount types.String `tfsdk:"service_account"`
	Tenant         types.String `tfsdk:"tenant"`
	ClientID       types.String `tfsdk:"client_id"`
	Scope          types.String `tfsdk:"scope"`
}

func newCloudAccountResource() resource.Resource {
	return &registration[cloudAccountModel]{
		typeName: "_secrets_cloud_account", route: "cloud-accounts", what: "cloud account",
		description: "A GCP service account or an Azure app that trusts roll's OIDC issuer for this project (docs/secrets.md, \"GCP and Azure\"): keys link to its access_token (`dynamic`). No key of the customer's is stored. roll changes none in place: every attribute replaces it.",
		schema: map[string]schema.Attribute{
			"name": reqString("Its name."),
			"cloud": schema.StringAttribute{Required: true, Description: "gcp (audience and service_account) or azure (tenant, client_id and optionally scope).",
				Validators:    []validator.String{stringvalidator.OneOf("gcp", "azure")},
				PlanModifiers: []planmodifier.String{stringplanmodifier.RequiresReplace()}},
			"audience":        optString("gcp: //iam.googleapis.com/projects/<number>/locations/global/workloadIdentityPools/<pool>/providers/<provider>."),
			"service_account": optString("gcp: name@project.iam.gserviceaccount.com, which the pool's principal may impersonate."),
			"tenant":          optString("azure: the Entra tenant."),
			"client_id":       optString("azure: the app's client id (a GUID)."),
			"scope": schema.StringAttribute{Optional: true, Computed: true, Description: "azure: what the token is for (https://management.azure.com/.default by default).",
				PlanModifiers: []planmodifier.String{stringplanmodifier.RequiresReplace(), stringplanmodifier.UseStateForUnknown()}},
		},
		keys: func(m *cloudAccountModel) (*types.String, *types.String) { return &m.Project, &m.ID },
		validate: func(_ context.Context, m *cloudAccountModel, d *diag.Diagnostics) {
			need := map[string][]string{"gcp": {"audience", "service_account"}, "azure": {"tenant", "client_id"}}
			not := map[string][]string{"gcp": {"tenant", "client_id", "scope"}, "azure": {"audience", "service_account"}}
			v := map[string]types.String{"audience": m.Audience, "service_account": m.ServiceAccount, "tenant": m.Tenant, "client_id": m.ClientID, "scope": m.Scope}
			cloud := m.Cloud.ValueString()
			for _, k := range need[cloud] {
				if unset(v[k]) {
					d.AddAttributeError(path.Root(k), "a "+cloud+" account needs "+k, "")
				}
			}
			for _, k := range not[cloud] {
				if !unset(v[k]) {
					d.AddAttributeError(path.Root(k), "a "+cloud+" account has no "+k, "")
				}
			}
		},
		body: func(_ context.Context, m *cloudAccountModel, _ func(string) types.String, _ *diag.Diagnostics) map[string]any {
			b := map[string]any{"name": m.Name.ValueString(), "cloud": m.Cloud.ValueString()}
			if m.Cloud.ValueString() == "gcp" {
				b["audience"], b["service_account"] = m.Audience.ValueString(), m.ServiceAccount.ValueString()
			} else {
				b["tenant"], b["client_id"] = m.Tenant.ValueString(), m.ClientID.ValueString()
				if !m.Scope.IsNull() && !m.Scope.IsUnknown() {
					b["scope"] = m.Scope.ValueString()
				}
			}
			return b
		},
		fill: func(m *cloudAccountModel, v map[string]any) {
			m.ID, m.Name, m.Cloud = jStr(v, "id"), jStr(v, "name"), jStr(v, "cloud")
			m.Audience, m.ServiceAccount, m.Tenant, m.ClientID, m.Scope = jStr(v, "audience"), jStr(v, "service_account"), jStr(v, "tenant"), jStr(v, "client_id"), jStr(v, "scope")
		},
	}
}
