import { KeyRound, Plus, RotateCw, Trash2 } from "lucide-react";
import { useState } from "react";
import type { SaasRotator, SaasVendor } from "@/lib/secrets-api";
import { Select } from "../agents/ui/select";
import { useSecrets } from "./secrets-page";
import { ActorMark, ago, button, card, dangerButton, ErrorNote, errorText, field, ghost, primary, Section, useLoad } from "./ui";

/**
 * A target field: text, a comma-separated `list`, a `number`, or one of `options`; `optional` ones
 * may stay empty (the server's default).
 */
type TargetField = { name: string; label: string; placeholder: string; optional?: boolean; kind?: "list" | "number"; options?: string[] };

/** Each vendor's target fields and what its credential is (docs/secrets.md, "SaaS rotators"). */
const VENDORS: Record<SaasVendor, { label: string; credential: string; fields: TargetField[] }> = {
  sendgrid: { label: "SendGrid", credential: "API key that may manage API keys", fields: [{ name: "scopes", label: "Scopes (optional, comma-separated)", placeholder: "mail.send", optional: true, kind: "list" }] },
  twilio: { label: "Twilio", credential: "Auth Token (or SK…:secret)", fields: [{ name: "account_sid", label: "Account SID", placeholder: "AC…" }] },
  okta: {
    label: "Okta",
    credential: "API token (SSWS)",
    fields: [
      { name: "domain", label: "Org domain", placeholder: "acme.okta.com" },
      { name: "app_id", label: "App id", placeholder: "0oa…" },
    ],
  },
  entra: {
    label: "Microsoft Entra",
    credential: "Client secret of the app roll signs in as",
    fields: [
      { name: "tenant", label: "Tenant", placeholder: "contoso.onmicrosoft.com" },
      { name: "client_id", label: "Client id roll signs in as", placeholder: "GUID" },
      { name: "object_id", label: "Object id of the app to rotate", placeholder: "GUID" },
    ],
  },
  auth0: {
    label: "Auth0",
    credential: "Client secret of the Management API app",
    fields: [
      { name: "domain", label: "Tenant domain", placeholder: "acme.us.auth0.com" },
      { name: "client_id", label: "Management API client id", placeholder: "client id" },
      { name: "app_client_id", label: "Client id of the app to rotate", placeholder: "client id" },
    ],
  },
  gitlab: {
    label: "GitLab",
    credential: "Access token (api scope, Maintainer or Owner)",
    fields: [
      { name: "host", label: "Host (optional)", placeholder: "gitlab.com", optional: true },
      { name: "project", label: "Project id or path", placeholder: "group/project" },
      { name: "scopes", label: "Scopes (comma-separated)", placeholder: "read_api", kind: "list" },
      { name: "access_level", label: "Access level (optional)", placeholder: "40", optional: true, kind: "number" },
      { name: "expires_days", label: "Expires after days (optional)", placeholder: "365", optional: true, kind: "number" },
    ],
  },
  openai: { label: "OpenAI", credential: "Admin API key", fields: [{ name: "project_id", label: "Project id", placeholder: "proj_…" }] },
  datadog: {
    label: "Datadog",
    credential: "API key:application key",
    fields: [
      { name: "site", label: "Site (optional)", placeholder: "datadoghq.com", optional: true },
      { name: "key", label: "Keys to rotate", placeholder: "api", options: ["api", "application"] },
      { name: "scopes", label: "Scopes (optional, application keys)", placeholder: "dashboards_read", optional: true, kind: "list" },
    ],
  },
  cloudflare: {
    label: "Cloudflare",
    credential: "API token that may manage API tokens",
    fields: [
      { name: "account_id", label: "Account id (optional; none: your user's tokens)", placeholder: "account id", optional: true },
      { name: "template", label: "Template token id (its policies are copied)", placeholder: "token id" },
    ],
  },
  databricks: {
    label: "Databricks",
    credential: "Personal access token of the identity whose tokens rotate",
    fields: [
      { name: "host", label: "Workspace host", placeholder: "dbc-a1b2c3d4-e5f6.cloud.databricks.com" },
      { name: "lifetime_days", label: "Lifetime in days (optional)", placeholder: "90", optional: true, kind: "number" },
    ],
  },
  confluent: {
    label: "Confluent Cloud",
    credential: "Cloud API key as KEY:SECRET",
    fields: [
      { name: "owner", label: "Owner", placeholder: "sa-…" },
      { name: "resource", label: "Resource (optional; none: Cloud API keys)", placeholder: "lkc-…", optional: true },
      { name: "environment", label: "Environment (optional, with a resource)", placeholder: "env-…", optional: true },
    ],
  },
};

/** What's shown of a target: its values, never the credential (the API doesn't have it to give). */
function targetText(t: Record<string, unknown>): string {
  return Object.entries(t)
    .map(([k, v]) => `${k}=${Array.isArray(v) ? v.join(",") : String(v)}`)
    .join(" ");
}

/**
 * SaaS rotators (docs/secrets.md, "SaaS rotators"): a vendor credential roll issues and revokes keys
 * with, through the vendor's own API. A key links to one (Link a resource) in managed mode.
 */
export function SaasRotators() {
  const { api } = useSecrets();
  const list = useLoad(() => api.saasRotators(), [api]);
  const [adding, setAdding] = useState(false);
  const [name, setName] = useState("");
  const [vendor, setVendor] = useState<SaasVendor>("sendgrid");
  const [target, setTarget] = useState<Record<string, string>>({});
  const [credential, setCredential] = useState("");
  // The day the registered credential ends at the vendor (optional): health warns two weeks before.
  const [ends, setEnds] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [deleting, setDeleting] = useState<string | null>(null);
  // Replacing a registration's credential in place: its id, the new credential and its end.
  const [replacing, setReplacing] = useState<string | null>(null);
  const [fresh, setFresh] = useState("");
  const [freshEnds, setFreshEnds] = useState("");
  const spec = VENDORS[vendor];
  const ok = /^[A-Za-z0-9_-]{1,64}$/.test(name) && credential.trim() !== "" && spec.fields.every((f) => f.optional || (target[f.name] ?? f.options?.[0] ?? "").trim() !== "");

  const add = async () => {
    setBusy(true);
    setError(null);
    const t: Record<string, unknown> = {};
    for (const f of spec.fields) {
      const v = (target[f.name] ?? f.options?.[0] ?? "").trim();
      if (f.kind === "list") {
        const items = v.split(",").map((s) => s.trim()).filter(Boolean);
        if (items.length) t[f.name] = items;
      } else if (v === "" && f.optional) continue;
      else t[f.name] = f.kind === "number" ? Number(v) : v;
    }
    try {
      await api.addSaasRotator({ name, vendor, target: t, credential: credential.trim(), ...(ends && { credential_expires_at: Date.parse(`${ends}T00:00:00Z`) }) });
      setAdding(false);
      setName("");
      setTarget({});
      setCredential("");
      setEnds("");
      list.reload();
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };
  const replace = async (r: SaasRotator) => {
    setBusy(true);
    setError(null);
    try {
      // Checked with the vendor first; a refused one leaves the old credential in place.
      await api.updateSaasRotator(r.id, { credential: fresh.trim(), credential_expires_at: freshEnds ? Date.parse(`${freshEnds}T00:00:00Z`) : null });
      setReplacing(null);
      setFresh("");
      setFreshEnds("");
      list.reload();
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };
  const del = (r: SaasRotator) =>
    api.deleteSaasRotator(r.id).then(
      () => (setDeleting(null), list.reload()),
      (e) => (setDeleting(null), setError(errorText(e))),
    );

  return (
    <Section
      title="SaaS rotators"
      aside={
        <button className={button} onClick={() => setAdding((a) => !a)}>
          <Plus className="size-3.5" /> Add a SaaS rotator
        </button>
      }
    >
      <p className="mb-2 text-[11.5px] text-muted">
        roll issues keys at SendGrid, Twilio, Okta, Entra, Auth0, GitLab, OpenAI, Datadog, Cloudflare, Databricks or Confluent itself, with a credential you register here, and revokes the old key after the grace period. Auth0 replaces a secret at once, so it has no grace period. Link a key to one from Link a resource. GitLab, Databricks and Entra end their keys by themselves: a link to one rotates at least two weeks before that.
      </p>
      <ErrorNote error={error ?? list.error} onDismiss={error ? () => setError(null) : undefined} className="mb-2" />
      {adding && (
        <form
          className="rise mb-3 flex flex-wrap items-end gap-2 rounded-xl border border-line-2 bg-panel p-3"
          onSubmit={(e) => {
            e.preventDefault();
            if (ok && !busy) void add();
          }}
        >
          <label className="w-40">
            <span className="mb-1 block text-[11px] font-medium text-subtle">Name</span>
            <input autoFocus value={name} onChange={(e) => setName(e.target.value)} placeholder="sendgrid" spellCheck={false} className={`${field} font-mono text-[12px]`} />
          </label>
          <div>
            <span className="mb-1 block text-[11px] font-medium text-subtle">Vendor</span>
            <Select
              label="Vendor"
              value={vendor}
              onChange={(v) => {
                setVendor(v as SaasVendor);
                setTarget({});
              }}
              options={(Object.keys(VENDORS) as SaasVendor[]).map((v) => ({ value: v, label: VENDORS[v].label }))}
              className="w-44"
            />
          </div>
          {spec.fields.map((f) =>
            f.options ? (
              <div key={f.name}>
                <span className="mb-1 block text-[11px] font-medium text-subtle">{f.label}</span>
                <Select label={f.label} value={target[f.name] ?? f.options[0]} onChange={(v) => setTarget((t) => ({ ...t, [f.name]: v }))} options={f.options.map((o) => ({ value: o, label: o }))} className="w-40" />
              </div>
            ) : (
              <label key={f.name} className="w-56">
                <span className="mb-1 block text-[11px] font-medium text-subtle">{f.label}</span>
                <input value={target[f.name] ?? ""} onChange={(e) => setTarget((t) => ({ ...t, [f.name]: e.target.value }))} placeholder={f.placeholder} inputMode={f.kind === "number" ? "numeric" : undefined} spellCheck={false} className={`${field} font-mono text-[12px]`} />
              </label>
            ),
          )}
          <label className="min-w-64 flex-1">
            <span className="mb-1 block text-[11px] font-medium text-subtle">{spec.credential} (kept sealed)</span>
            <input type="password" autoComplete="off" value={credential} onChange={(e) => setCredential(e.target.value)} spellCheck={false} className={`${field} font-mono text-[12px]`} />
          </label>
          <label className="w-40">
            <span className="mb-1 block text-[11px] font-medium text-subtle">Credential expires (optional)</span>
            <input type="date" value={ends} onChange={(e) => setEnds(e.target.value)} className={`${field} font-mono text-[12px]`} />
          </label>
          <button type="submit" className={primary} disabled={!ok || busy}>
            {busy ? "Adding…" : "Add"}
          </button>
          <button type="button" className={ghost} onClick={() => setAdding(false)}>
            Cancel
          </button>
        </form>
      )}
      {list.data && list.data.length === 0 && !adding ? (
        <p className="rounded-xl border border-dashed border-line-2 px-3 py-4 text-[12px] text-muted">No SaaS rotators. Add one to have roll issue and rotate a vendor&apos;s keys.</p>
      ) : (
        list.data && (
          <div className={`${card} divide-y divide-line`}>
            {list.data.map((r) => (
              <div key={r.id} className="flex flex-wrap items-center gap-x-3 gap-y-1 px-3 py-2">
                <RotateCw className="size-3.5 text-muted" />
                <span className="font-mono text-[12px]">{r.name}</span>
                <span className="text-[11.5px] text-muted">{VENDORS[r.vendor]?.label ?? r.vendor}</span>
                <span className="min-w-0 truncate font-mono text-[11.5px] text-subtle">{targetText(r.target)}</span>
                {r.credential_expires_at !== undefined && <span className="font-mono text-[11px] text-subtle">credential expires {new Date(r.credential_expires_at).toISOString().slice(0, 10)}</span>}
                <span className="ml-auto text-[11px] text-subtle">
                  added {ago(r.created_at)} · <ActorMark who={r.created_by} />
                </span>
                <button className={ghost} onClick={() => setReplacing((x) => (x === r.id ? null : r.id))} aria-label={`Replace the credential of ${r.name}`} title="Replace credential">
                  <KeyRound className="size-3.5" />
                </button>
                {deleting === r.id ? (
                  <span className="flex gap-1">
                    <button className={dangerButton} onClick={() => del(r)}>
                      Delete
                    </button>
                    <button className={ghost} onClick={() => setDeleting(null)}>
                      Keep
                    </button>
                  </span>
                ) : (
                  <button className={ghost} onClick={() => setDeleting(r.id)} aria-label={`Delete the SaaS rotator ${r.name}`}>
                    <Trash2 className="size-3.5" />
                  </button>
                )}
                {replacing === r.id && (
                  <form
                    className="rise flex w-full flex-wrap items-end gap-2 pt-1"
                    onSubmit={(e) => {
                      e.preventDefault();
                      if (fresh.trim() && !busy) void replace(r);
                    }}
                  >
                    <label className="min-w-64 flex-1">
                      <span className="mb-1 block text-[11px] font-medium text-subtle">New {VENDORS[r.vendor]?.credential ?? "credential"} (kept sealed; linked keys keep working)</span>
                      <input autoFocus type="password" autoComplete="off" value={fresh} onChange={(e) => setFresh(e.target.value)} spellCheck={false} className={`${field} font-mono text-[12px]`} />
                    </label>
                    <label className="w-40">
                      <span className="mb-1 block text-[11px] font-medium text-subtle">Credential expires (optional)</span>
                      <input type="date" value={freshEnds} onChange={(e) => setFreshEnds(e.target.value)} className={`${field} font-mono text-[12px]`} />
                    </label>
                    <button type="submit" className={primary} disabled={!fresh.trim() || busy}>
                      {busy ? "Checking…" : "Replace credential"}
                    </button>
                    <button type="button" className={ghost} onClick={() => setReplacing(null)}>
                      Cancel
                    </button>
                  </form>
                )}
              </div>
            ))}
          </div>
        )
      )}
    </Section>
  );
}
