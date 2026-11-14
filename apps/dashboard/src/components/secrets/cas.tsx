import { Check, Copy, FileSignature, ListChecks, Plus, ShieldCheck, Trash2 } from "lucide-react";
import { useState } from "react";
import type { Ca, SignedCertificate } from "@/lib/secrets-api";
import { useSecrets } from "./secrets-page";
import { ActorMark, ago, button, card, dangerButton, ErrorNote, errorText, field, ghost, primary, Section, useLoad } from "./ui";

const DOMAIN = /^(?!-)[a-z0-9-]{1,63}(\.(?!-)[a-z0-9-]{1,63})*$/;

/**
 * Certificate authorities (docs/secrets.md, "Certificates"): a CA of the project's own that signs
 * short-lived TLS certificates for its domains, for mTLS between services. Its key never leaves
 * roll; link TLS_CERT, TLS_KEY and TLS_CA to it (Link a resource) with the names a certificate is for.
 */
export function Cas() {
  const { api } = useSecrets();
  const list = useLoad(() => api.cas(), [api]);
  const [adding, setAdding] = useState(false);
  const [name, setName] = useState("");
  const [domains, setDomains] = useState("");
  const [allowIp, setAllowIp] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [deleting, setDeleting] = useState<string | null>(null);
  const [copied, setCopied] = useState<string | null>(null);
  const [signing, setSigning] = useState<string | null>(null);
  const [viewing, setViewing] = useState<string | null>(null);
  const list_ = domains.split(",").map((d) => d.trim().toLowerCase()).filter(Boolean);
  const ok = /^[A-Za-z0-9_-]{1,64}$/.test(name) && list_.length > 0 && list_.every((d) => DOMAIN.test(d));

  const add = async () => {
    setBusy(true);
    setError(null);
    try {
      await api.addCa({ name, domains: list_, allow_ip: allowIp });
      setAdding(false);
      setName("");
      setDomains("");
      setAllowIp(false);
      list.reload();
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };
  const del = (c: Ca) =>
    api.deleteCa(c.id).then(
      () => (setDeleting(null), list.reload()),
      (e) => (setDeleting(null), setError(errorText(e))),
    );
  const copy = (c: Ca) => {
    void navigator.clipboard?.writeText(c.certificate);
    setCopied(c.id);
    setTimeout(() => setCopied(null), 1200);
  };

  return (
    <Section
      title="Certificate authorities"
      aside={
        <button className={button} onClick={() => setAdding((a) => !a)}>
          <Plus className="size-3.5" /> Add a CA
        </button>
      }
    >
      <p className="mb-2 text-[11.5px] text-muted">
        Short-lived TLS certificates for services, signed by a CA of this project&apos;s own whose key never leaves roll. Link <span className="font-mono">TLS_CERT</span>,{" "}
        <span className="font-mono">TLS_KEY</span> and <span className="font-mono">TLS_CA</span> to it with the names a certificate is for: dynamic gives each lease its own, managed rotates one.
        Clients trust the CA&apos;s certificate, which is limited to its domains.
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
            <input autoFocus value={name} onChange={(e) => setName(e.target.value)} placeholder="internal" spellCheck={false} className={`${field} font-mono text-[12px]`} />
          </label>
          <label className="min-w-56 flex-1">
            <span className="mb-1 block text-[11px] font-medium text-subtle">Domains it signs for (comma-separated)</span>
            <input value={domains} onChange={(e) => setDomains(e.target.value)} placeholder="svc.internal" spellCheck={false} className={`${field} font-mono text-[12px]`} />
          </label>
          <label className="flex h-8 items-center gap-1.5 text-[12px] text-muted">
            <input type="checkbox" checked={allowIp} onChange={(e) => setAllowIp(e.target.checked)} /> IP addresses too
          </label>
          <button type="submit" className={primary} disabled={!ok || busy}>
            {busy ? "Making it…" : "Make CA"}
          </button>
          <button type="button" className={ghost} onClick={() => setAdding(false)}>
            Cancel
          </button>
        </form>
      )}
      {list.data && list.data.length === 0 && !adding ? (
        <p className="rounded-xl border border-dashed border-line-2 px-3 py-4 text-[12px] text-muted">No certificate authorities. Make one to give services TLS certificates without running a CA.</p>
      ) : (
        list.data && (
          <div className={`${card} divide-y divide-line`}>
            {list.data.map((c) => (
              <div key={c.id} className="flex flex-wrap items-center gap-x-3 gap-y-1 px-3 py-2">
                <ShieldCheck className="size-3.5 text-muted" />
                <span className="font-mono text-[12px]">{c.name}</span>
                <span className="min-w-0 truncate font-mono text-[11.5px] text-subtle">{c.domains.join(", ")}{c.allow_ip ? " · IPs" : ""}</span>
                <span className="text-[11px] text-subtle">up to {c.max_days} days</span>
                <span className="ml-auto text-[11px] text-subtle">
                  made {ago(c.created_at)} · <ActorMark who={c.created_by} />
                </span>
                <button className={ghost} onClick={() => (setSigning(null), setViewing((v) => (v === c.id ? null : c.id)))} aria-expanded={viewing === c.id} aria-label={`Certificates ${c.name} signed`} title="What it signed that hasn't expired; revoke one">
                  <ListChecks className="size-3.5" />
                </button>
                <button className={ghost} onClick={() => (setViewing(null), setSigning((s) => (s === c.id ? null : c.id)))} aria-expanded={signing === c.id} aria-label={`Sign a CSR with ${c.name}`} title="Sign a CSR: the device or mesh keeps its private key">
                  <FileSignature className="size-3.5" />
                </button>
                <button className={ghost} onClick={() => copy(c)} aria-label={`Copy ${c.name}'s certificate`} title="Copy the CA's certificate (PEM) for clients to trust">
                  {copied === c.id ? <Check className="size-3.5 text-ok" /> : <Copy className="size-3.5" />}
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
                  <button className={ghost} onClick={() => setDeleting(c.id)} aria-label={`Delete the CA ${c.name}`}>
                    <Trash2 className="size-3.5" />
                  </button>
                )}
                {signing === c.id && <SignCsr ca={c} />}
                {viewing === c.id && <Certificates ca={c} />}
              </div>
            ))}
          </div>
        )
      )}
    </Section>
  );
}

/** Paste a CSR, get its certificate: names within the CA's policy, its key stays where it was made. */
function SignCsr({ ca }: { ca: Ca }) {
  const { api } = useSecrets();
  const [csr, setCsr] = useState("");
  const [days, setDays] = useState(Math.min(30, ca.max_days));
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [signed, setSigned] = useState<SignedCertificate | null>(null);
  const [copied, setCopied] = useState(false);
  const ok = csr.includes("BEGIN CERTIFICATE REQUEST") && days >= 1 && days <= ca.max_days;
  const sign = async () => {
    setBusy(true);
    setError(null);
    try {
      setSigned(await api.signCsr(ca.id, csr.trim(), days));
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="rise basis-full space-y-2 pt-1">
      <ErrorNote error={error} onDismiss={() => setError(null)} />
      {signed ? (
        <>
          <p className="text-[11.5px] text-muted">
            For <span className="font-mono text-fg">{signed.names.join(", ")}</span> until {new Date(signed.not_after).toISOString().slice(0, 10)} · serial <span className="font-mono">{signed.serial.slice(0, 16)}…</span>
          </p>
          <textarea readOnly value={signed.certificate} rows={6} className={`${field} h-auto py-1.5 font-mono text-[11px]`} aria-label="The signed certificate" />
          <div className="flex gap-2">
            <button
              className={button}
              onClick={() => {
                void navigator.clipboard?.writeText(signed.certificate);
                setCopied(true);
                setTimeout(() => setCopied(false), 1200);
              }}
            >
              {copied ? <Check className="size-3.5 text-ok" /> : <Copy className="size-3.5" />} Copy certificate
            </button>
            <button className={ghost} onClick={() => (setSigned(null), setCsr(""))}>
              Sign another
            </button>
          </div>
        </>
      ) : (
        <form
          className="space-y-2"
          onSubmit={(e) => {
            e.preventDefault();
            if (ok && !busy) void sign();
          }}
        >
          <textarea
            autoFocus
            value={csr}
            onChange={(e) => setCsr(e.target.value)}
            rows={5}
            spellCheck={false}
            placeholder={"-----BEGIN CERTIFICATE REQUEST-----\n…"}
            aria-label="The CSR (PEM)"
            className={`${field} h-auto py-1.5 font-mono text-[11px]`}
          />
          <div className="flex flex-wrap items-center gap-2">
            <label className="flex items-center gap-1.5 text-[12px] text-muted">
              Valid for
              <input type="number" min={1} max={ca.max_days} value={days} onChange={(e) => setDays(Number(e.target.value))} className={`${field} h-7 w-16 font-mono text-[12px]`} aria-label="Days" />
              days
            </label>
            <span className="text-[11px] text-subtle">Only its key and subjectAltName names are used; each must be under {ca.domains.join(", ")}.</span>
            <button type="submit" className={`${primary} ml-auto`} disabled={!ok || busy}>
              {busy ? "Signing…" : "Sign"}
            </button>
          </div>
        </form>
      )}
    </div>
  );
}

/** What a CA signed that hasn't expired; revoking one puts it on the CA's CRL. */
function Certificates({ ca }: { ca: Ca }) {
  const { api } = useSecrets();
  const list = useLoad(() => api.caCertificates(ca.id), [api, ca.id]);
  const [confirm, setConfirm] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const revoke = (serial: string) =>
    api.revokeCertificate(ca.id, serial).then(
      () => (setConfirm(null), list.reload()),
      (e) => (setConfirm(null), setError(errorText(e))),
    );
  const day = (ms: number) => new Date(ms).toISOString().slice(0, 10);
  return (
    <div className="rise basis-full space-y-2 pt-1">
      <ErrorNote error={error ?? list.error} onDismiss={error ? () => setError(null) : undefined} />
      {list.data && list.data.length === 0 && <p className="text-[11.5px] text-muted">Nothing it signed is still valid.</p>}
      {list.data && list.data.length > 0 && (
        <div className="divide-y divide-line rounded-lg border border-line">
          {list.data.map((x) => (
            <div key={x.serial} className="flex flex-wrap items-center gap-x-3 gap-y-0.5 px-2.5 py-1.5 text-[11.5px]">
              <span className="font-mono text-fg">{x.names.join(", ")}</span>
              <span className="rounded border border-line-2 px-1 text-[10.5px] text-subtle">{x.source === "csr" ? "CSR" : "link"}</span>
              <span className="font-mono text-[10.5px] text-subtle" title={x.serial}>
                {x.serial.slice(0, 12)}…
              </span>
              {x.revoked_at ? (
                <span className="ml-auto text-danger">revoked {day(x.revoked_at)}</span>
              ) : (
                <>
                  <span className="ml-auto text-subtle">until {day(x.not_after)}</span>
                  {confirm === x.serial ? (
                    <span className="flex gap-1">
                      <button className={dangerButton} onClick={() => void revoke(x.serial)}>
                        Revoke
                      </button>
                      <button className={ghost} onClick={() => setConfirm(null)}>
                        Keep
                      </button>
                    </span>
                  ) : (
                    <button className={`${ghost} h-6`} onClick={() => setConfirm(x.serial)} title="Put it on the CA's revocation list until it expires">
                      Revoke
                    </button>
                  )}
                </>
              )}
            </div>
          ))}
        </div>
      )}
      <p className="text-[11px] text-subtle">
        Clients that check revocation fetch the CA&apos;s list at <span className="font-mono">/v1/secrets/crl/{ca.id}</span>, which every certificate names.
      </p>
    </div>
  );
}
