import { Check, Copy, Plus, Server, Terminal, Trash2 } from "lucide-react";
import { useState } from "react";
import type { SshCa } from "@/lib/secrets-api";
import { useSecrets } from "./secrets-page";
import { ActorMark, ago, button, card, dangerButton, ErrorNote, errorText, field, ghost, primary, Section, useLoad } from "./ui";

const LOGIN = /^[A-Za-z0-9._][A-Za-z0-9._-]{0,63}$/;
const DOMAIN = /^(?!-)[a-z0-9-]{1,63}(\.(?!-)[a-z0-9-]{1,63})*$/;

/**
 * SSH CAs (docs/secrets.md, "SSH certificates"): a project's own SSH CA that signs short-lived user
 * certificates for the logins its policy allows. Servers trust its public key (TrustedUserCAKeys)
 * instead of everyone's keys; link SSH_CERT and SSH_KEY to it with the logins a certificate is for.
 */
export function SshCas() {
  const { api } = useSecrets();
  const list = useLoad(() => api.sshCas(), [api]);
  const [adding, setAdding] = useState(false);
  const [name, setName] = useState("");
  const [logins, setLogins] = useState("");
  const [hours, setHours] = useState("8");
  const [hostDomains, setHostDomains] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [deleting, setDeleting] = useState<string | null>(null);
  const [copied, setCopied] = useState<string | null>(null);
  const principals = logins.split(",").map((l) => l.trim()).filter(Boolean);
  const domains = hostDomains.split(",").map((d) => d.trim().toLowerCase()).filter(Boolean);
  const h = Number(hours);
  const ok = /^[A-Za-z0-9_-]{1,64}$/.test(name) && principals.length + domains.length > 0 && principals.every((p) => LOGIN.test(p)) && domains.every((d) => DOMAIN.test(d)) && Number.isInteger(h) && h >= 1 && h <= 168;

  const add = async () => {
    setBusy(true);
    setError(null);
    try {
      await api.addSshCa({ name, principals, max_hours: h, host_domains: domains });
      setAdding(false);
      setName("");
      setLogins("");
      setHostDomains("");
      list.reload();
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };
  const del = (c: SshCa) =>
    api.deleteSshCa(c.id).then(
      () => (setDeleting(null), list.reload()),
      (e) => (setDeleting(null), setError(errorText(e))),
    );
  const copy = (c: SshCa, what: "key" | "hosts") => {
    void navigator.clipboard?.writeText(what === "key" ? c.public_key : (c.known_hosts ?? ""));
    setCopied(`${c.id}:${what}`);
    setTimeout(() => setCopied(null), 1200);
  };

  return (
    <Section
      title="SSH certificate authorities"
      aside={
        <button className={button} onClick={() => setAdding((a) => !a)}>
          <Plus className="size-3.5" /> Add an SSH CA
        </button>
      }
    >
      <p className="mb-2 text-[11.5px] text-muted">
        Short-lived SSH user certificates instead of long-lived keys on every server: servers trust the CA&apos;s public key (<span className="font-mono">TrustedUserCAKeys</span>), and each lease of{" "}
        <span className="font-mono">SSH_CERT</span> and <span className="font-mono">SSH_KEY</span> gets a key and certificate for the logins you link, ending with it. With host domains, it also signs
        servers&apos; host keys (<span className="font-mono">roll secrets ssh-cas sign-host</span>), and clients trust them through its <span className="font-mono">known_hosts</span> line instead of on first use.
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
            <input autoFocus value={name} onChange={(e) => setName(e.target.value)} placeholder="ops" spellCheck={false} className={`${field} font-mono text-[12px]`} />
          </label>
          <label className="min-w-56 flex-1">
            <span className="mb-1 block text-[11px] font-medium text-subtle">Logins it signs for (comma-separated)</span>
            <input value={logins} onChange={(e) => setLogins(e.target.value)} placeholder="deploy, ubuntu" spellCheck={false} className={`${field} font-mono text-[12px]`} />
          </label>
          <label className="min-w-48 flex-1">
            <span className="mb-1 block text-[11px] font-medium text-subtle">Host domains it signs servers for (optional)</span>
            <input value={hostDomains} onChange={(e) => setHostDomains(e.target.value)} placeholder="svc.internal" spellCheck={false} className={`${field} font-mono text-[12px]`} />
          </label>
          <label className="w-28">
            <span className="mb-1 block text-[11px] font-medium text-subtle">Longest (hours)</span>
            <input value={hours} onChange={(e) => setHours(e.target.value)} inputMode="numeric" className={`${field} font-mono text-[12px]`} />
          </label>
          <button type="submit" className={primary} disabled={!ok || busy}>
            {busy ? "Making it…" : "Make SSH CA"}
          </button>
          <button type="button" className={ghost} onClick={() => setAdding(false)}>
            Cancel
          </button>
        </form>
      )}
      {list.data && list.data.length === 0 && !adding ? (
        <p className="rounded-xl border border-dashed border-line-2 px-3 py-4 text-[12px] text-muted">No SSH CAs. Make one to give jobs and people short-lived SSH certificates.</p>
      ) : (
        list.data && (
          <div className={`${card} divide-y divide-line`}>
            {list.data.map((c) => (
              <div key={c.id} className="flex flex-wrap items-center gap-x-3 gap-y-1 px-3 py-2">
                <Terminal className="size-3.5 text-muted" />
                <span className="font-mono text-[12px]">{c.name}</span>
                <span className="min-w-0 truncate font-mono text-[11.5px] text-subtle">
                  {[...c.principals, ...(c.host_domains ?? []).map((d) => `hosts under ${d}`)].join(", ")}
                </span>
                <span className="text-[11px] text-subtle">up to {c.max_hours} h</span>
                <span className="ml-auto text-[11px] text-subtle">
                  made {ago(c.created_at)} · <ActorMark who={c.created_by} />
                </span>
                {c.known_hosts && (
                  <button className={ghost} onClick={() => copy(c, "hosts")} aria-label={`Copy ${c.name}'s known_hosts line`} title="Copy the known_hosts line (@cert-authority) that trusts the servers it signs; sign a server's key with roll secrets ssh-cas sign-host">
                    {copied === `${c.id}:hosts` ? <Check className="size-3.5 text-ok" /> : <Server className="size-3.5" />}
                  </button>
                )}
                <button className={ghost} onClick={() => copy(c, "key")} aria-label={`Copy ${c.name}'s public key`} title="Copy the CA's public key, for servers' TrustedUserCAKeys">
                  {copied === `${c.id}:key` ? <Check className="size-3.5 text-ok" /> : <Copy className="size-3.5" />}
                </button>
                {deleting === c.id ? (
                  <span className="flex gap-1">
                    <button className={dangerButton} onClick={() => del(c)}>
                      Delete
                    </button>
                    <button className={ghost} onClick={() => setDeleting(null)}>
                      Keep
                    </button>
                  </span>
                ) : (
                  <button className={ghost} onClick={() => setDeleting(c.id)} aria-label={`Delete the SSH CA ${c.name}`}>
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
