/**
 * A project's tokens are roll-access's (docs/secrets.md, "Access"; docs/permissions.md): a token made
 * from the Secrets page is a grant with one rule on this project's secrets. These turn the page's
 * form into that rule, and a grant back into the row the page shows.
 */
import type { Grant, NewGrant, Rule } from "./access-api";
import type { Action, NewToken, Token } from "@roll/secrets/types";

const DAY = 86_400_000;

/** The grant `POST /v1/access/tokens` makes for `t`: one rule on `project`'s secrets. */
export function grantFor(project: string, t: NewToken, now = Date.now()): NewGrant {
  const rule: Rule = {
    product: "secrets",
    resources: [{ project }],
    actions: Object.fromEntries(t.actions.map((a) => [a, "allow"])),
  };
  if (t.environments?.length) rule.environments = t.environments;
  if (t.paths?.length) rule.paths = t.paths;
  const g: NewGrant = { name: t.name, rules: [rule] };
  if (t.expires_at) g.expires_at = t.expires_at;
  else if (t.ttl_days) g.expires_at = now + t.ttl_days * DAY;
  if (t.allowed_ips?.length) g.ip_allow = t.allowed_ips;
  return g;
}

/** A grant as the Secrets page shows it: what its rule on `project` reaches. */
export function tokenOf(g: Grant, project: string): Token {
  // The rule on this project: naming it (`{project}`), naming it by id (`{id}`, perhaps only some
  // encryption keys: `{id, index}`), or everything (`*`), the specific one first.
  const secrets = g.rules.filter((r) => r.product === "secrets" || r.product === "*");
  const names = (r: (typeof secrets)[number]) => r.resources !== "*" && r.resources.some((s) => ("project" in s && s.project === project) || ("id" in s && s.id === project));
  const rule = secrets.find(names) ?? secrets.find((r) => r.resources === "*");
  const selector = rule && rule.resources !== "*" ? rule.resources.find((s) => "id" in s && s.id === project) : undefined;
  const keys = selector && "index" in selector && selector.index ? selector.index : null;
  const actions = Object.entries(rule?.actions ?? {})
    .filter(([, e]) => e === "allow")
    .map(([a]) => a as Action);
  return {
    id: g.id,
    name: g.name,
    environments: rule?.environments ?? null,
    paths: rule?.paths ?? null,
    actions,
    expires_at: g.expires_at,
    created_at: g.created_at,
    created_by: g.created_by,
    last_used_at: g.last_used_at,
    allowed_ips: g.ip_allow.length ? g.ip_allow : null,
    keys,
  };
}
