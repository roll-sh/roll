import { Cloud, Plus, Trash2 } from "lucide-react";
import { useState } from "react";
import { secretsIssuer, type CloudAccount, type NewCloudAccount } from "@/lib/secrets-api";
import { useSecrets } from "./secrets-page";
import { ActorMark, ago, button, card, dangerButton, ErrorNote, errorText, field, ghost, primary, Section, useLoad } from "./ui";

type CloudKind = "gcp" | "azure";

/**
 * GCP and Azure (docs/secrets.md, "GCP and Azure"): accounts that trust roll's own token for this
 * project, so each lease of a dynamic link gets a cloud access token, with no key of yours stored.
 * Registering one proves its trust names this project and no other.
 */
export function CloudAccounts() {
  const { api, project } = useSecrets();
  const list = useLoad(() => api.cloudAccounts(), [api]);
  const issuer = useLoad(() => secretsIssuer(), []);
  const [adding, setAdding] = useState<CloudKind | null>(null);
  const [name, setName] = useState("");
  const [a, setA] = useState("");
  const [b, setB] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [deleting, setDeleting] = useState<string | null>(null);
  const subject = `roll-secrets:${project.id}`;
  const ok =
    /^[A-Za-z0-9_-]{1,64}$/.test(name) &&
    (adding === "gcp" ? a.startsWith("//iam.googleapis.com/projects/") && b.endsWith(".gserviceaccount.com") : adding === "azure" && a.trim() !== "" && /^[0-9a-f-]{36}$/i.test(b.trim()));

  const add = async () => {
    if (!adding) return;
    setBusy(true);
    setError(null);
    const body: NewCloudAccount = adding === "gcp" ? { name, cloud: "gcp", audience: a.trim(), service_account: b.trim() } : { name, cloud: "azure", tenant: a.trim(), client_id: b.trim() };
    try {
      await api.addCloudAccount(body);
      setAdding(null);
      setName("");
      setA("");
      setB("");
      list.reload();
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };
  const del = (x: CloudAccount) =>
    api.deleteCloudAccount(x.id).then(
      () => (setDeleting(null), list.reload()),
      (e) => (setDeleting(null), setError(errorText(e))),
    );
  const open = (c: CloudKind) => {
    setAdding((cur) => (cur === c ? null : c));
    setA("");
    setB("");
  };

  return (
    <Section
      title="GCP and Azure"
      aside={
        <span className="flex gap-1.5">
          <button className={button} onClick={() => open("gcp")}>
            <Plus className="size-3.5" /> GCP
          </button>
          <button className={button} onClick={() => open("azure")}>
            <Plus className="size-3.5" /> Azure
          </button>
        </span>
      }
    >
      <p className="mb-2 text-[11.5px] text-muted">
        A cloud access token for each lease, with no key of yours stored: your cloud trusts roll&apos;s own token for this project. Trust the issuer{" "}
        <span className="font-mono select-all">{issuer.data?.issuer ?? "…"}</span> with the subject <span className="font-mono select-all">{subject}</span> (GCP: a workload identity pool provider whose
        condition pins <span className="font-mono">assertion.sub</span>; Azure: a federated credential, audience <span className="font-mono">api://AzureADTokenExchange</span>). Then link a key to the account&apos;s{" "}
        <span className="font-mono">access_token</span>, dynamic.
      </p>
      {issuer.data && !issuer.data.issuer.startsWith("https://") && (
        <p className="mb-2 rounded-lg border border-line-2 bg-panel px-3 py-2 text-[11.5px] text-muted">
          Clouds reach roll&apos;s issuer only over HTTPS at a public address: this server&apos;s is {issuer.data.issuer}. Set rolld&apos;s <span className="font-mono">ROLL_CODE_PUBLIC_URL</span> first.
        </p>
      )}
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
            <input autoFocus value={name} onChange={(e) => setName(e.target.value)} placeholder={adding === "gcp" ? "analytics" : "infra"} spellCheck={false} className={`${field} font-mono text-[12px]`} />
          </label>
          <label className="min-w-64 flex-1">
            <span className="mb-1 block text-[11px] font-medium text-subtle">{adding === "gcp" ? "Pool provider" : "Tenant"}</span>
            <input
              value={a}
              onChange={(e) => setA(e.target.value)}
              placeholder={adding === "gcp" ? "//iam.googleapis.com/projects/123/locations/global/workloadIdentityPools/roll/providers/roll" : "contoso.onmicrosoft.com"}
              spellCheck={false}
              className={`${field} font-mono text-[12px]`}
            />
          </label>
          <label className="min-w-56 flex-1">
            <span className="mb-1 block text-[11px] font-medium text-subtle">{adding === "gcp" ? "Service account" : "App (client) id"}</span>
            <input value={b} onChange={(e) => setB(e.target.value)} placeholder={adding === "gcp" ? "reports@shop.iam.gserviceaccount.com" : "11111111-2222-3333-4444-555555555555"} spellCheck={false} className={`${field} font-mono text-[12px]`} />
          </label>
          <button type="submit" className={primary} disabled={!ok || busy}>
            {busy ? "Checking its trust…" : `Add ${adding === "gcp" ? "GCP" : "Azure"} account`}
          </button>
          <button type="button" className={ghost} onClick={() => setAdding(null)}>
            Cancel
          </button>
        </form>
      )}
      {list.data && list.data.length === 0 && !adding ? (
        <p className="rounded-xl border border-dashed border-line-2 px-3 py-4 text-[12px] text-muted">No cloud accounts. Add one to give services GCP or Azure tokens that end with their lease.</p>
      ) : (
        list.data && (
          <div className={`${card} divide-y divide-line`}>
            {list.data.map((x) => (
              <div key={x.id} className="flex flex-wrap items-center gap-x-3 gap-y-1 px-3 py-2">
                <Cloud className="size-3.5 text-muted" />
                <span className="font-mono text-[12px]">{x.name}</span>
                <span className="rounded border border-line-2 px-1 text-[10px] text-subtle">{x.cloud === "gcp" ? "GCP" : "Azure"}</span>
                <span className="min-w-0 truncate font-mono text-[11.5px] text-subtle">{x.cloud === "gcp" ? x.service_account : `${x.client_id} in ${x.tenant}`}</span>
                <span className="ml-auto text-[11px] text-subtle">
                  added {ago(x.created_at)} · <ActorMark who={x.created_by} />
                </span>
                {deleting === x.id ? (
                  <span className="flex gap-1">
                    <button className={dangerButton} onClick={() => del(x)}>
                      Delete
                    </button>
                    <button className={ghost} onClick={() => setDeleting(null)}>
                      Keep
                    </button>
                  </span>
                ) : (
                  <button className={ghost} onClick={() => setDeleting(x.id)} aria-label={`Delete the cloud account ${x.name}`}>
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
