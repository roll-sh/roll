import { Plus, Ship, Trash2 } from "lucide-react";
import { useState } from "react";
import type { KubernetesCluster } from "@/lib/secrets-api";
import { useSecrets } from "./secrets-page";
import { ActorMark, ago, button, card, dangerButton, ErrorNote, errorText, field, ghost, primary, Section, useLoad } from "./ui";

/**
 * Kubernetes clusters (docs/secrets.md, "Kubernetes service-account tokens"): roll issues a
 * short-lived token of an existing service account per lease, through the TokenRequest API. The
 * admin token and the cluster's CA are sealed; a token can't be revoked, so it lives until it expires.
 */
export function KubernetesClusters() {
  const { api } = useSecrets();
  const list = useLoad(() => api.kubernetesClusters(), [api]);
  const [adding, setAdding] = useState(false);
  const [name, setName] = useState("");
  const [url, setUrl] = useState("");
  const [token, setToken] = useState("");
  const [ca, setCa] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [deleting, setDeleting] = useState<string | null>(null);
  const ok = /^[A-Za-z0-9_-]{1,64}$/.test(name) && /^https:\/\/[^\s/@?#]+(\/\S*)?$/.test(url.trim()) && /^\S+$/.test(token.trim());

  const add = async () => {
    setBusy(true);
    setError(null);
    try {
      await api.addKubernetesCluster({ name, api_url: url.trim(), token: token.trim(), ca_pem: ca.trim() || undefined });
      setAdding(false);
      setName("");
      setUrl("");
      setToken("");
      setCa("");
      list.reload();
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };
  const del = (k: KubernetesCluster) =>
    api.deleteKubernetesCluster(k.id).then(
      () => (setDeleting(null), list.reload()),
      (e) => (setDeleting(null), setError(errorText(e))),
    );

  return (
    <Section
      title="Kubernetes clusters"
      aside={
        <button className={button} onClick={() => setAdding((a) => !a)}>
          <Plus className="size-3.5" /> Add a cluster
        </button>
      }
    >
      <p className="mb-2 text-[11.5px] text-muted">
        Short-lived service account tokens, one per lease. Link <span className="font-mono">KUBE_TOKEN</span> to a cluster (dynamic) with the namespace and service account it&apos;s for, and{" "}
        <span className="font-mono">KUBE_API_URL</span> and <span className="font-mono">KUBE_CA</span> to build a kubeconfig. A token can&apos;t be revoked: it works until it expires, so keep leases short.
      </p>
      <ErrorNote error={error ?? list.error} onDismiss={error ? () => setError(null) : undefined} className="mb-2" />
      {adding && (
        <form
          className="rise mb-3 grid gap-2 rounded-xl border border-line-2 bg-panel p-3 sm:grid-cols-[11rem_1fr]"
          onSubmit={(e) => {
            e.preventDefault();
            if (ok && !busy) void add();
          }}
        >
          <label>
            <span className="mb-1 block text-[11px] font-medium text-subtle">Name</span>
            <input autoFocus value={name} onChange={(e) => setName(e.target.value)} placeholder="prod-eu" spellCheck={false} className={`${field} font-mono text-[12px]`} />
          </label>
          <label>
            <span className="mb-1 block text-[11px] font-medium text-subtle">API server (https, a public address)</span>
            <input value={url} onChange={(e) => setUrl(e.target.value)} placeholder="https://ABC123.gr7.eu-west-1.eks.amazonaws.com" spellCheck={false} autoComplete="off" className={`${field} font-mono text-[12px]`} />
          </label>
          <label className="sm:col-start-2">
            <span className="mb-1 block text-[11px] font-medium text-subtle">Admin token (kept sealed; a service account that may create tokens)</span>
            <input type="password" value={token} onChange={(e) => setToken(e.target.value)} placeholder="a service account token" spellCheck={false} autoComplete="off" className={`${field} font-mono text-[12px]`} />
          </label>
          <details className="sm:col-start-2">
            <summary className="cursor-pointer text-[11px] text-subtle select-none">Cluster CA certificate (when the API server&apos;s isn&apos;t publicly trusted)</summary>
            <textarea value={ca} onChange={(e) => setCa(e.target.value)} rows={4} placeholder="-----BEGIN CERTIFICATE-----" spellCheck={false} className={`${field} mt-1 font-mono text-[11px]`} />
          </details>
          <div className="sm:col-start-2 flex gap-2">
            <button type="submit" className={primary} disabled={!ok || busy}>
              {busy ? "Connecting…" : "Add cluster"}
            </button>
            <button type="button" className={ghost} onClick={() => setAdding(false)}>
              Cancel
            </button>
          </div>
        </form>
      )}
      {list.data && list.data.length === 0 && !adding ? (
        <p className="rounded-xl border border-dashed border-line-2 px-3 py-4 text-[12px] text-muted">No Kubernetes clusters. Add one to hand out service account tokens that expire on their own.</p>
      ) : (
        list.data && (
          <div className={`${card} divide-y divide-line`}>
            {list.data.map((k) => (
              <div key={k.id} className="flex flex-wrap items-center gap-x-3 gap-y-1 px-3 py-2">
                <Ship className="size-3.5 text-muted" />
                <span className="font-mono text-[12px]">{k.name}</span>
                <span className="min-w-0 truncate font-mono text-[11.5px] text-subtle">{k.api_url}</span>
                {k.has_ca && <span className="text-[11px] text-muted">its own CA</span>}
                <span className="ml-auto text-[11px] text-subtle">
                  added {ago(k.created_at)} · <ActorMark who={k.created_by} />
                </span>
                {deleting === k.id ? (
                  <span className="flex gap-1">
                    <button className={dangerButton} onClick={() => del(k)}>
                      Delete
                    </button>
                    <button className={ghost} onClick={() => setDeleting(null)}>
                      Keep
                    </button>
                  </span>
                ) : (
                  <button className={ghost} onClick={() => setDeleting(k.id)} aria-label={`Delete the cluster ${k.name}`}>
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
