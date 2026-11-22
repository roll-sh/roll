import { useEffect, useState } from "react";
import type { Environment, Secret } from "@/lib/secrets-api";
import { DEFAULT_HEADER, hostsBody, hostsForm, type HostsForm } from "@/lib/secrets-hosts";
import { Dialog } from "../agents/ui/dialog";
import { Segmented } from "../agents/ui/toggle-group";
import { useSecrets } from "./secrets-page";
import { dangerButton, ErrorNote, errorText, field, ghost, label, primary, textareaField } from "./ui";

/**
 * Bind a key to hosts (docs/secrets.md, "Using a key without seeing it"), a project admin's: a sandbox or a fork's
 * preview then holds a placeholder, and roll's proxy swaps in the value only on the way to these hosts, only in the
 * header or query parameter named here (`Authorization: Bearer {}` unless changed).
 */
export function HostsDialog({ open, onClose, env, path, secret, onSaved }: { open: boolean; onClose: () => void; env: Environment; path: string; secret: Secret | null; onSaved: () => void }) {
  const { api } = useSecrets();
  const [form, setForm] = useState<HostsForm>(() => hostsForm(secret?.hosts));
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    if (open) {
      setForm(hostsForm(secret?.hosts));
      setError(null);
    }
  }, [open, secret]);
  const body = hostsBody(form);
  const problem = form.hosts.trim() && typeof body === "string" ? body : null;
  const key = secret?.key ?? "";
  const run = async (call: () => Promise<unknown>) => {
    setBusy(true);
    setError(null);
    try {
      await call();
      onSaved();
      onClose();
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };
  const save = () => typeof body !== "string" && run(() => api.setHosts(env.slug, key, body, path));
  const unbind = () => run(() => api.clearHosts(env.slug, key, path));
  const p = form.placement;
  return (
    <Dialog
      open={open}
      onClose={onClose}
      busy={busy}
      title={`Bind ${key} to hosts`}
      lead="Sandboxes and previews that ask for placeholders get one instead of the value. roll's proxy swaps the value in only on the way to these hosts, only where the placement says."
      footer={
        <>
          {secret?.hosts && (
            <button className={`${dangerButton} mr-auto`} onClick={unbind} disabled={busy}>
              Unbind
            </button>
          )}
          <button className={ghost} onClick={onClose} disabled={busy}>
            Cancel
          </button>
          <button className={primary} onClick={save} disabled={busy || typeof body === "string"}>
            {busy ? "Saving…" : "Save"}
          </button>
        </>
      }
    >
      <div className="space-y-3 text-[12.5px]">
        <label className="block">
          <span className={label}>Hosts</span>
          <textarea className={`${textareaField} font-mono`} rows={3} value={form.hosts} onChange={(e) => setForm({ ...form, hosts: e.target.value })} placeholder={"api.stripe.com\n*.stripe.com"} spellCheck={false} autoFocus />
        </label>
        <div>
          <span className={label}>Where the key goes</span>
          <Segmented
            label="Where the key goes"
            value={p.kind}
            onChange={(kind) => setForm({ ...form, placement: kind === "header" ? { kind, header: DEFAULT_HEADER } : { kind, query: "api_key" } })}
            options={[
              { value: "header", label: "A header" },
              { value: "query", label: "A query parameter" },
            ]}
          />
          <input
            className={`${field} mt-2 font-mono`}
            value={p.kind === "header" ? p.header : p.query}
            onChange={(e) => setForm({ ...form, placement: p.kind === "header" ? { kind: "header", header: e.target.value } : { kind: "query", query: e.target.value } })}
            spellCheck={false}
            aria-invalid={problem !== null}
            aria-label={p.kind === "header" ? "Header template" : "Query parameter"}
          />
          <p className="mt-1 text-[11.5px] text-muted">{p.kind === "header" ? "{} is where the value goes. A placeholder in any other header blocks the request." : "Only as this parameter's whole value."}</p>
        </div>
        {problem && <p className="text-[11.5px] text-danger">{problem}</p>}
        <label className="flex items-start gap-2 text-[12px]">
          <input type="checkbox" className="mt-0.5" checked={form.untrusted} onChange={(e) => setForm({ ...form, untrusted: e.target.checked })} />
          <span>
            Untrusted code too <span className="text-muted">(a fork's preview, an agent's sandbox): for a sensitive key, only with this on.</span>
          </span>
        </label>
        <ErrorNote error={error} onDismiss={() => setError(null)} />
      </div>
    </Dialog>
  );
}
