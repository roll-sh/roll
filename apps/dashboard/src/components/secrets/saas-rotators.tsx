import { Plus, RotateCw, Trash2 } from "lucide-react";
import { useState } from "react";
import type { SaasRotator, SaasVendor } from "@/lib/secrets-api";
import { Select } from "../agents/ui/select";
import { useSecrets } from "./secrets-page";
import { ActorMark, ago, button, card, dangerButton, ErrorNote, errorText, field, ghost, primary, Section, useLoad } from "./ui";

/** Each vendor's target fields and what its credential is (docs/secrets.md, "SaaS rotators"). */
const VENDORS: Record<SaasVendor, { label: string; credential: string; fields: { name: string; label: string; placeholder: string }[] }> = {
  sendgrid: { label: "SendGrid", credential: "API key that may manage API keys", fields: [{ name: "scopes", label: "Scopes (optional, comma-separated)", placeholder: "mail.send" }] },
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
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [deleting, setDeleting] = useState<string | null>(null);
  const spec = VENDORS[vendor];
  const ok = /^[A-Za-z0-9_-]{1,64}$/.test(name) && credential.trim() !== "" && spec.fields.every((f) => f.name === "scopes" || (target[f.name] ?? "").trim() !== "");

  const add = async () => {
    setBusy(true);
    setError(null);
    const t: Record<string, unknown> = {};
    for (const f of spec.fields) {
      const v = (target[f.name] ?? "").trim();
      if (f.name === "scopes") {
        const scopes = v.split(",").map((s) => s.trim()).filter(Boolean);
        if (scopes.length) t.scopes = scopes;
      } else t[f.name] = v;
    }
    try {
      await api.addSaasRotator({ name, vendor, target: t, credential: credential.trim() });
      setAdding(false);
      setName("");
      setTarget({});
      setCredential("");
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
        roll issues keys at SendGrid, Twilio, Okta, Entra or Auth0 itself, with a credential you register here, and revokes the old key after the grace period. Auth0 replaces a secret at once, so it has no grace period. Link a key to one from Link a resource.
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
          {spec.fields.map((f) => (
            <label key={f.name} className="w-56">
              <span className="mb-1 block text-[11px] font-medium text-subtle">{f.label}</span>
              <input value={target[f.name] ?? ""} onChange={(e) => setTarget((t) => ({ ...t, [f.name]: e.target.value }))} placeholder={f.placeholder} spellCheck={false} className={`${field} font-mono text-[12px]`} />
            </label>
          ))}
          <label className="min-w-64 flex-1">
            <span className="mb-1 block text-[11px] font-medium text-subtle">{spec.credential} (kept sealed)</span>
            <input type="password" autoComplete="off" value={credential} onChange={(e) => setCredential(e.target.value)} spellCheck={false} className={`${field} font-mono text-[12px]`} />
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
                <span className="ml-auto text-[11px] text-subtle">
                  added {ago(r.created_at)} · <ActorMark who={r.created_by} />
                </span>
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
              </div>
            ))}
          </div>
        )
      )}
    </Section>
  );
}
